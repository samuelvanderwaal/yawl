use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug, Serialize, Deserialize)]
pub enum YawlError {
    #[error("No instructions to transmit")]
    NoInstructions,

    #[error("Failed to create the client: {0}")]
    FailedToCreateClient(String),

    #[error("No recent blockhash")]
    NoRecentBlockhash,

    #[error("Send batch transaction failed: {0}")]
    BatchTransactionError(String),

    #[error("Transaction Error")]
    TransactionError(String),

    #[error("No signers found")]
    NoSigners,
}
