//! Generic PoSW Miner and Verifier, compatible with any implementer of the SNARK trait.
use crate::circuit::{POSWCircuit, POSWCircuitParameters};
use snarkos_algorithms::crh::sha256d_to_u64;
use snarkos_curves::{
    bls12_377::Fr,
    edwards_bls12::{EdwardsProjective, Fq},
};
use snarkos_errors::{curves::constraint_field::ConstraintFieldError, parameters::ParametersError, algorithms::SNARKError};
use snarkos_gadgets::{algorithms::crh::PedersenCompressedCRHGadget, curves::edwards_bls12::EdwardsBlsGadget};
use snarkos_models::{
    algorithms::{MerkleParameters, SNARK},
    curves::{to_field_vec::ToConstraintField, PrimeField},
    gadgets::algorithms::MaskedCRHGadget,
    parameters::Parameters,
};
use snarkos_objects::{
    pedersen_merkle_tree::{pedersen_merkle_root_hash_with_leaves, PedersenMerkleRootHash, PARAMS},
    MaskedMerkleTreeParameters,
    ProofOfSuccinctWork,
};
use snarkos_parameters::posw::{PoswProvingParameters, PoswVerificationParameters};
use snarkos_profiler::{end_timer, start_timer};
use snarkos_utilities::{
    bytes::{FromBytes, ToBytes},
    to_bytes,
};

use blake2::{digest::Digest, Blake2s};
use rand::Rng;
use std::{io::Error as IoError, marker::PhantomData};
use thiserror::Error;

// We need to instantiate the Merkle Tree and the Gadget, but these should not be
// proving system specific
pub type M = MaskedMerkleTreeParameters;
pub type HG = PedersenCompressedCRHGadget<EdwardsProjective, Fq, EdwardsBlsGadget>;
pub type F = Fr;

#[derive(Debug, Error)]
pub enum PoswError {
    #[error("could not load PoSW parameters: {0}")]
    Parameters(#[from] ParametersError),

    #[error("could not verify PoSW")]
    PoswVerificationFailed,

    #[error(transparent)]
    SnarkError(#[from] SNARKError),

    #[error(transparent)]
    IoError(#[from] IoError),

    #[error(transparent)]
    ConstraintFieldError(#[from] ConstraintFieldError)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posw<S, F, M, HG, CP>
where
    S: SNARK,
    F: PrimeField,
    M: MerkleParameters,
    HG: MaskedCRHGadget<M::H, F>,
    CP: POSWCircuitParameters,
{
    circuit: PhantomData<POSWCircuit<F, M, HG, CP>>,
    pub vk: S::PreparedVerificationParameters,
    pub pk: Option<S::ProvingParameters>,
}

impl<S, CP> Posw<S, F, M, HG, CP>
where
    S: SNARK<VerifierInput = Vec<F>, Circuit = POSWCircuit<F, M, HG, CP>, AssignedCircuit = POSWCircuit<F, M, HG, CP>>,
    CP: POSWCircuitParameters,
{
    /// Verification only mode of the circuit (used by non-mining nodes)
    pub fn load(verify_only: bool) -> Result<Self, PoswError> {
        let params = PoswVerificationParameters::load_bytes()?;
        let vk = S::VerificationParameters::read(&params[..])?;

        let pk = if verify_only {
            None
        } else {
            let params = PoswProvingParameters::load_bytes()?;
            Some(S::ProvingParameters::read(&params[..])?)
        };

        Ok(Self {
            pk,
            vk: vk.into(),
            circuit: PhantomData,
        })
    }

    /// Performs a trusted setup for the PoSW circuit and returns an instance of the
    /// miner
    pub fn setup<R: Rng>(rng: &mut R) -> Result<Self, PoswError> {
        let params = S::setup(
            POSWCircuit {
                // the circuit will be padded internally
                leaves: vec![None; 0],
                merkle_parameters: PARAMS.clone(),
                mask: None,
                root: None,
                field_type: PhantomData,
                crh_gadget_type: PhantomData,
                circuit_parameters_type: PhantomData,
            },
            rng,
        )?;
        Ok(Self {
            pk: Some(params.0),
            vk: params.1,
            circuit: PhantomData,
        })
    }

    /// Given the subroots of the block, it will calculate a POSW and a nonce which fit
    /// the difficulty target. These can then be used in the block header's field.
    pub fn mine<R: Rng>(
        &self,
        subroots: Vec<Vec<u8>>,
        difficulty_target: u64, // TODO: Change to Bignum?
        rng: &mut R,
        max_nonce: u32,
    ) -> Result<(u32, ProofOfSuccinctWork), PoswError> {
        let pk = self.pk.as_ref().expect("tried to mine without a PK set up");
        let mut nonce;
        let mut proof;
        loop {
            nonce = rng.gen_range(0, max_nonce);
            proof = Self::prove(&pk, nonce, &subroots, rng)?;

            if self.check_difficulty(&proof, difficulty_target) {
                break;
            }
        }

        Ok((nonce, proof))
    }

    /// Given a nonce and the thing you're grinding the nonce over, this should generate
    /// the proof
    fn prove<R: Rng>(
        pk: &S::ProvingParameters,
        nonce: u32,
        subroots: &[Vec<u8>],
        rng: &mut R,
    ) -> Result<ProofOfSuccinctWork, PoswError> {
        // instantiate the circuit with the nonce
        let circuit = Self::circuit_from(nonce, subroots);

        // generate the proof
        let proof_timer = start_timer!(|| "POSW proof");
        let proof = S::prove(pk, circuit, rng)?;
        end_timer!(proof_timer);

        // serialize it
        let proof_bytes = to_bytes![proof]?;
        let mut p = [0; ProofOfSuccinctWork::size()];
        p.copy_from_slice(&proof_bytes);
        Ok(ProofOfSuccinctWork(p))
    }

    // hash the proof and check it against the difficulty
    fn check_difficulty(&self, proof: &ProofOfSuccinctWork, difficulty_target: u64) -> bool {
        let hash_result = sha256d_to_u64(&proof.0[..]);
        hash_result <= difficulty_target
    }

    /// TODO: FIX THIS!
    pub fn verify(
        &self,
        nonce: u32,
        proof: &ProofOfSuccinctWork,
        pedersen_merkle_root: &PedersenMerkleRootHash,
    ) -> Result<(), PoswError> {
        // deserialize the snark proof ASAP
        let proof = <S as SNARK>::Proof::read(&proof.0[..])?;

        // commit to it and the nonce
        let mask = commit(nonce, pedersen_merkle_root);

        // get the mask and the root in public inputs format
        let merkle_root = F::read(&pedersen_merkle_root.0[..])?;
        let inputs = [mask.to_field_elements()?, vec![merkle_root]].concat();

        let res = S::verify(&self.vk, &inputs, &proof)?;
        if !res {
            return Err(PoswError::PoswVerificationFailed);
        }

        Ok(())
    }

    pub fn circuit_from(nonce: u32, leaves: &[Vec<u8>]) -> POSWCircuit<F, M, HG, CP> {
        let (root, leaves) = pedersen_merkle_root_hash_with_leaves(leaves);

        // Generate the mask by committing to the nonce and the root
        let mask = commit(nonce, &root.into());

        // Convert the leaves to Options for the SNARK
        let leaves = leaves.into_iter().map(|l| Some(l)).collect();

        POSWCircuit {
            leaves,
            merkle_parameters: PARAMS.clone(),
            mask: Some(mask),
            root: Some(root),
            field_type: PhantomData,
            crh_gadget_type: PhantomData,
            circuit_parameters_type: PhantomData,
        }
    }
}

/// Commits to the nonce and pedersen merkle root
pub fn commit(nonce: u32, root: &PedersenMerkleRootHash) -> Vec<u8> {
    let mut h = Blake2s::new();
    h.input(&nonce.to_le_bytes());
    h.input(root.0.as_ref());
    h.result().to_vec()
}
