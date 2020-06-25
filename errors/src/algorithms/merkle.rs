use crate::algorithms::CRHError;

#[derive(Debug, Error)]
pub enum MerkleError {
    #[error("{}: {}", _0, _1)]
    Crate(&'static str, String),

    #[error("{}", _0)]
    CRHError(CRHError),

    #[error("Incorrect leaf index: {}", _0)]
    IncorrectLeafIndex(usize),

    #[error("Incorrect path length: {}", _0)]
    IncorrectPathLength(usize),

    #[error("Invalid path length: {}. Must be less than: {}", _0, _1)]
    InvalidPathLength(usize, usize),

    #[error("Invalid tree height: {}. Must be less than or equal to: {}", _0, _1)]
    InvalidTreeHeight(usize, usize),

    #[error("{}", _0)]
    Message(String),

    #[error("No leaves given to Merkle tree")]
    NoLeaves,
}

impl From<CRHError> for MerkleError {
    fn from(error: CRHError) -> Self {
        MerkleError::CRHError(error)
    }
}

impl From<std::io::Error> for MerkleError {
    fn from(error: std::io::Error) -> Self {
        MerkleError::Crate("std::io", format!("{:?}", error))
    }
}
