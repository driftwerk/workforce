//! A source of one bank connection's accounts, read only.

use async_trait::async_trait;

use super::model::{Account, Balance, BankError, Connection, Page, Window};

/// One bank, as its provider shows it. Every method only reads.
#[async_trait]
pub trait BankProvider: Send + Sync {
    /// The bank the connection reads, and whether its approval stands.
    async fn connection(&self) -> Result<Connection, BankError>;
    async fn accounts(&self) -> Result<Vec<Account>, BankError>;
    async fn balance(&self, account: &str) -> Result<Balance, BankError>;
    /// One page of `account`'s transactions in `window`, from `cursor`.
    async fn transactions(
        &self,
        account: &str,
        window: Window,
        cursor: Option<&str>,
    ) -> Result<Page, BankError>;
    /// When the person last approved the bank, as Desk recorded it.
    fn consented_at(&self) -> Option<chrono::DateTime<chrono::Utc>>;
}
