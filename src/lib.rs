use futures_util::{stream::FuturesOrdered, StreamExt};
use indicatif::ProgressBar;
use ratelimit::Ratelimiter;
use serde::{Deserialize, Serialize};
use solana_client::{client_error::reqwest::Url, nonblocking::rpc_client::RpcClient};
use solana_program::{instruction::Instruction, message::Message};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};

use std::{sync::Arc, time::Duration};

mod error;

pub use error::YawlError;

const MAX_TX_LEN: usize = 1232;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum YawlResult {
    /// The transaction was successful and contains the signature of the transaction.
    Success(Signature),
    /// The transaction failed and contains the transaction and error message.
    Failure(YawlFailedTransaction),
}

impl YawlResult {
    /// Returns true if the result is a success.
    pub fn is_success(&self) -> bool {
        match self {
            YawlResult::Success(_) => true,
            YawlResult::Failure(_) => false,
        }
    }

    /// Returns true if the result is a failure.
    pub fn is_failure(&self) -> bool {
        !self.is_success()
    }

    /// Parses the transaction from a failure or returns None if the result is a success.
    pub fn message(&self) -> Option<Message> {
        match self {
            YawlResult::Success(_) => None,
            YawlResult::Failure(f) => Some(f.message.clone()),
        }
    }

    /// Parses the error message from a failure or returns None if the result is a success.
    pub fn error(&self) -> Option<String> {
        match self {
            YawlResult::Success(_) => None,
            YawlResult::Failure(f) => Some(f.error.clone()),
        }
    }

    /// Parses the signature from a success or returns None if the result is a failure.
    pub fn signature(&self) -> Option<String> {
        match self {
            YawlResult::Success(s) => Some(s.to_string()),
            YawlResult::Failure(_) => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct YawlFailedTransaction {
    pub message: Message,
    pub error: String,
}

pub struct Yawl {
    client: Arc<RpcClient>,
    instructions: Vec<Instruction>,
    signers: Vec<Keypair>,
    compute_budget: u32,
    priority_fee: u64,
    rate_limit: u64,
}

impl Yawl {
    pub fn new(rpc_url: Url, instructions: Vec<Instruction>, signers: Vec<Keypair>) -> Self {
        let client = Arc::new(RpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig::confirmed(),
        ));

        Self {
            client,
            instructions,
            signers,
            compute_budget: 200_000,
            priority_fee: 0,
            rate_limit: 10,
        }
    }

    pub fn add_instruction(&mut self, ix: Instruction) {
        self.instructions.push(ix);
    }

    pub fn add_signer(&mut self, signer: Keypair) {
        self.signers.push(signer);
    }

    pub fn add_compute_budget(&mut self, compute_budget: u32) {
        self.compute_budget = compute_budget;
    }

    pub fn add_priority_fee(&mut self, priority_fee: u64) {
        self.priority_fee = priority_fee;
    }

    pub fn set_rate_limit(&mut self, rate_limit: u64) {
        self.rate_limit = rate_limit;
    }

    /// Pack the instructions into transactions. This will return a vector of transactions that can be submitted to the network.
    pub async fn pack(&mut self) -> Result<Vec<Transaction>, YawlError> {
        if self.instructions.is_empty() {
            return Err(YawlError::NoInstructions);
        }

        let mut packed_transactions = Vec::new();

        let mut instructions = Vec::new();
        let payer_pubkey = self.signers.first().ok_or(YawlError::NoSigners)?.pubkey();

        if self.compute_budget != 200_000 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                self.compute_budget,
            ));
        }
        if self.priority_fee != 0 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                self.priority_fee,
            ));
        }

        let mut current_transaction =
            Transaction::new_with_payer(&instructions, Some(&payer_pubkey));
        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let latest_blockhash = self
            .client
            .get_latest_blockhash()
            .await
            .map_err(|_| YawlError::NoRecentBlockhash)?;

        for ix in self.instructions.iter_mut() {
            instructions.push(ix.clone());
            let mut tx = Transaction::new_with_payer(&instructions, Some(&payer_pubkey));
            tx.sign(&signers, latest_blockhash);

            let tx_len = bincode::serialize(&tx).unwrap().len();

            if tx_len > MAX_TX_LEN || tx.message.account_keys.len() > 64 {
                packed_transactions.push(current_transaction.clone());

                // Clear instructions except for the last one that pushed the transaction over the size limit.
                // Check for compute budget and priority fees again and add them to the front of the instructions.
                instructions = vec![];
                if self.compute_budget != 200_000 {
                    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                        self.compute_budget,
                    ));
                }
                if self.priority_fee != 0 {
                    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                        self.priority_fee,
                    ));
                }
                instructions.push(ix.clone());
            } else {
                current_transaction = tx;
            }
        }
        packed_transactions.push(current_transaction);

        Ok(packed_transactions)
    }

    /// Used to repack failed transactions into new transactions with a fresh blockhash and signatures.
    /// These transacations can be resubmitted with `sail_with_transactions`.
    pub async fn repack_failed(
        &mut self,
        failed_transactions: Vec<YawlFailedTransaction>,
    ) -> Result<Vec<Transaction>, YawlError> {
        let mut packed_transactions = Vec::new();

        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        for tx in failed_transactions {
            let mut tx = Transaction::new_unsigned(tx.message);
            let blockhash = self
                .client
                .get_latest_blockhash()
                .await
                .map_err(|_| YawlError::NoRecentBlockhash)?;
            tx.sign(&signers, blockhash);

            packed_transactions.push(tx);
        }

        Ok(packed_transactions)
    }

    /// Pack the instructions and submit them to the network. This will return a vector of results.
    pub async fn sail(&mut self) -> Result<Vec<YawlResult>, YawlError> {
        let packed_transactions = self.pack().await?;

        let results = self._sail(packed_transactions).await?;

        Ok(results)
    }

    /// Submit a vector of transactions to the network. This will return a vector of results.
    /// This can be used to retry failed transactions.
    pub async fn sail_with_transactions(
        &mut self,
        packed_transactions: Vec<Transaction>,
    ) -> Result<Vec<YawlResult>, YawlError> {
        let results = self._sail(packed_transactions).await?;

        Ok(results)
    }

    async fn _sail(
        &mut self,
        packed_transactions: Vec<Transaction>,
    ) -> Result<Vec<YawlResult>, YawlError> {
        let mut tasks = FuturesOrdered::new();

        let pb = ProgressBar::new(packed_transactions.len() as u64);

        let ratelimiter = Ratelimiter::builder(self.rate_limit, Duration::from_secs(1))
            .max_tokens(self.rate_limit)
            .initial_available(self.rate_limit)
            .build()
            .unwrap();

        for tx in packed_transactions {
            let pb = pb.clone();
            let client = Arc::clone(&self.client);

            // Wait for the ratelimiter to allow the transaction to be sent.
            if let Err(sleep) = ratelimiter.try_wait() {
                tokio::time::sleep(sleep).await;
                continue;
            }

            let task = tokio::spawn(async move {
                let res = client.send_and_confirm_transaction(&tx.clone()).await;
                pb.inc(1);
                match res {
                    Ok(signature) => YawlResult::Success(signature),
                    Err(e) => YawlResult::Failure(YawlFailedTransaction {
                        message: tx.message.clone(),
                        error: e.to_string(),
                    }),
                }
            });
            tasks.push_back(task);
        }

        let mut results = Vec::new();

        while let Some(result) = tasks.next().await {
            match result {
                Ok(result) => results.push(result),
                Err(e) => {
                    return Err(YawlError::TransactionError(e.to_string()));
                }
            }
        }

        Ok(results)
    }
}
