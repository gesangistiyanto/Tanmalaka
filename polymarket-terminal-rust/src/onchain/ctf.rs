//! On-chain interactions with the Polymarket ConditionalTokens (CTF) contracts.
//!
//! Supports both wallet modes:
//!   EOA   — signs transactions directly from the EOA
//!   PROXY — routes through the Gnosis Safe proxy wallet
//!
//! Key operations:
//!   token_balance   — read ERC1155 balance of a CTF token
//!   usdc_balance    — read USDC.e balance of the wallet
//!   merge_positions — return equal YES+NO → USDC (recover entry cost)
//!   redeem_positions — claim USDC after market resolution

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::contract::Interface;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, keccak256, Address, Bytes, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{bail, Context, Result};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::{Config, WalletMode};

// ── Contract addresses (Polygon mainnet) ─────────────────────────────────────

pub const CTF_ADDRESS: Address = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
pub const USDC_ADDRESS: Address = address!("2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
pub const CTF_EXCHANGE: Address = address!("4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");
pub const NEG_RISK_EXCHANGE: Address = address!("C5d563A36AE78145C45a50134d48A1215220f80a");
pub const GNOSIS_SAFE_ABI_EXEC_TX: &str = "execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes)";

// ── Solidity interfaces ───────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface ICTF {
        function mergePositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata partition,
            uint256 amount
        ) external;

        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
        ) external;

        function balanceOf(address account, uint256 id) external view returns (uint256);

        function payoutDenominator(bytes32 conditionId) external view returns (uint256);

        function isApprovedForAll(address account, address operator) external view returns (bool);

        function setApprovalForAll(address operator, bool approved) external;
    }

    #[sol(rpc)]
    interface IERC20 {
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IGnosisSafe {
        function nonce() external view returns (uint256);
        function getTransactionHash(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            uint256 _nonce
        ) external view returns (bytes32);
        function execTransaction(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            bytes calldata signatures
        ) external payable returns (bool);
    }
}

// ── Provider type alias ───────────────────────────────────────────────────────

type HttpProvider = alloy::providers::fillers::FillProvider<
    alloy::providers::fillers::JoinFill<
        alloy::providers::fillers::JoinFill<
            alloy::providers::Identity,
            alloy::providers::fillers::JoinFill<
                alloy::providers::fillers::GasFiller,
                alloy::providers::fillers::JoinFill<
                    alloy::providers::fillers::BlobGasFiller,
                    alloy::providers::fillers::JoinFill<
                        alloy::providers::fillers::NonceFiller,
                        alloy::providers::fillers::ChainIdFiller,
                    >,
                >,
            >,
        >,
        alloy::providers::fillers::WalletFiller<alloy::network::EthereumWallet>,
    >,
    alloy::providers::RootProvider<alloy::transports::http::Http<reqwest::Client>>,
    alloy::transports::http::Http<reqwest::Client>,
    alloy::network::Ethereum,
>;

// ── CtfClient ─────────────────────────────────────────────────────────────────

/// Serializes on-chain calls to prevent nonce collisions.
pub struct CtfClient {
    provider: HttpProvider,
    signer: PrivateKeySigner,
    config: Config,
    wallet_address: Address,
    /// Mutex serializes on-chain calls (nonces are sequential).
    tx_lock: Mutex<()>,
    usdc_approved: tokio::sync::RwLock<bool>,
    exchange_approved: dashmap::DashSet<String>,
}

impl CtfClient {
    pub async fn new(signer: PrivateKeySigner, config: &Config) -> Result<Arc<Self>> {
        let eoa_str = format!("{}", signer.address());
        let wallet_str = config.wallet_address(&eoa_str);
        let wallet_address: Address = wallet_str
            .parse()
            .context("Invalid wallet address")?;

        let wallet = EthereumWallet::from(signer.clone());
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(config.polygon_rpc_url.parse()?);

        Ok(Arc::new(Self {
            provider,
            signer,
            config: config.clone(),
            wallet_address,
            tx_lock: Mutex::new(()),
            usdc_approved: tokio::sync::RwLock::new(false),
            exchange_approved: dashmap::DashSet::new(),
        }))
    }

    // ── Read operations ───────────────────────────────────────────────────────

    /// ERC1155 balance of a CTF token for our wallet.
    pub async fn token_balance(&self, token_id: &str) -> Result<f64> {
        let token_uint = U256::from_str_radix(token_id, 10)
            .context("Invalid token ID")?;

        let ctf = ICTF::new(CTF_ADDRESS, &self.provider);
        let bal = ctf
            .balanceOf(self.wallet_address, token_uint)
            .call()
            .await
            .context("balanceOf failed")?
            ._0;

        Ok(bal.to::<u128>() as f64 / 1_000_000.0)
    }

    /// USDC.e balance of our wallet.
    pub async fn usdc_balance(&self) -> Result<f64> {
        let usdc = IERC20::new(USDC_ADDRESS, &self.provider);
        let bal = usdc
            .balanceOf(self.wallet_address)
            .call()
            .await
            .context("USDC balanceOf failed")?
            ._0;
        Ok(bal.to::<u128>() as f64 / 1_000_000.0)
    }

    /// Check whether the CTF market is resolved (payoutDenominator != 0).
    pub async fn is_resolved(&self, condition_id_hex: &str) -> Result<bool> {
        let bytes: FixedBytes<32> = condition_id_hex.parse().context("Invalid conditionId")?;
        let ctf = ICTF::new(CTF_ADDRESS, &self.provider);
        let denom = ctf
            .payoutDenominator(bytes)
            .call()
            .await
            .context("payoutDenominator failed")?
            ._0;
        Ok(!denom.is_zero())
    }

    // ── Write operations ──────────────────────────────────────────────────────

    /// Merge `shares` equal YES+NO tokens back to USDC.
    /// Amount is floored to 6 decimal places to avoid exceeding on-chain balance.
    pub async fn merge_positions(&self, condition_id_hex: &str, shares: f64, neg_risk: bool) -> Result<()> {
        if self.config.dry_run {
            info!("CTF[SIM]: merge {:.4} YES+NO → ${:.4} USDC", shares, shares);
            return Ok(());
        }

        // Floor to 6 decimal precision to avoid "exceeds balance" revert
        let shares_floored = (shares * 1_000_000.0).floor() / 1_000_000.0;
        let amount = U256::from((shares_floored * 1_000_000.0) as u64);

        let condition_id: FixedBytes<32> = condition_id_hex.parse().context("Invalid conditionId")?;

        let ctf = ICTF::new(CTF_ADDRESS, &self.provider);
        let calldata = ICTF::mergePositionsCall {
            collateralToken: USDC_ADDRESS,
            parentCollectionId: FixedBytes::ZERO,
            conditionId: condition_id,
            partition: vec![U256::from(1u64), U256::from(2u64)],
            amount,
        }
        .abi_encode();

        self.execute_call(CTF_ADDRESS, calldata.into(), "mergePositions", 500_000).await
    }

    /// Redeem tokens after market resolution.
    pub async fn redeem_positions(&self, condition_id_hex: &str, neg_risk: bool) -> Result<()> {
        if self.config.dry_run {
            info!("CTF[SIM]: redeem for conditionId {}", &condition_id_hex[..12.min(condition_id_hex.len())]);
            return Ok(());
        }

        if !self.is_resolved(condition_id_hex).await.unwrap_or(false) {
            bail!("Market not resolved yet — cannot redeem");
        }

        let condition_id: FixedBytes<32> = condition_id_hex.parse().context("Invalid conditionId")?;

        let calldata = ICTF::redeemPositionsCall {
            collateralToken: USDC_ADDRESS,
            parentCollectionId: FixedBytes::ZERO,
            conditionId: condition_id,
            indexSets: vec![U256::from(1u64), U256::from(2u64)],
        }
        .abi_encode();

        self.execute_call(CTF_ADDRESS, calldata.into(), "redeemPositions", 500_000).await
    }

    /// Ensure USDC is approved to the CTF contract (one-time per wallet).
    pub async fn ensure_usdc_approval(&self) -> Result<()> {
        if *self.usdc_approved.read().await {
            return Ok(());
        }

        let usdc = IERC20::new(USDC_ADDRESS, &self.provider);
        let allowance = usdc
            .allowance(self.wallet_address, CTF_ADDRESS)
            .call()
            .await?
            ._0;

        let max_u256 = U256::MAX;
        if allowance >= max_u256 / U256::from(2u64) {
            *self.usdc_approved.write().await = true;
            return Ok(());
        }

        info!("CTF: approving USDC → CTF contract...");
        let calldata = IERC20::approveCall {
            spender: CTF_ADDRESS,
            amount: U256::MAX,
        }
        .abi_encode();

        self.execute_call(USDC_ADDRESS, calldata.into(), "approve USDC", 100_000).await?;
        *self.usdc_approved.write().await = true;
        info!("CTF: USDC approved");
        Ok(())
    }

    /// Ensure the CTF exchange is an approved ERC1155 operator (one-time per wallet).
    pub async fn ensure_exchange_approval(&self, neg_risk: bool) -> Result<()> {
        let exchange = if neg_risk { NEG_RISK_EXCHANGE } else { CTF_EXCHANGE };
        let key = format!("{}", exchange);

        if self.exchange_approved.contains(&key) {
            return Ok(());
        }

        let ctf = ICTF::new(CTF_ADDRESS, &self.provider);
        let approved = ctf
            .isApprovedForAll(self.wallet_address, exchange)
            .call()
            .await?
            ._0;

        if approved {
            self.exchange_approved.insert(key);
            return Ok(());
        }

        info!("CTF: approving CTF exchange as ERC1155 operator...");
        let calldata = ICTF::setApprovalForAllCall {
            operator: exchange,
            approved: true,
        }
        .abi_encode();

        self.execute_call(CTF_ADDRESS, calldata.into(), "setApprovalForAll", 100_000).await?;
        self.exchange_approved.insert(key);
        info!("CTF: CTF exchange approved");
        Ok(())
    }

    // ── Internal tx dispatch ──────────────────────────────────────────────────

    /// Serialized transaction execution — routes to EOA or Safe depending on wallet mode.
    async fn execute_call(
        &self,
        to: Address,
        data: Bytes,
        description: &str,
        gas_limit: u64,
    ) -> Result<()> {
        let _guard = self.tx_lock.lock().await;
        info!("CTF: executing {} ...", description);

        match self.config.wallet_mode {
            WalletMode::Eoa => self.eoa_call(to, data, description, gas_limit).await,
            WalletMode::Proxy => self.safe_call(to, data, description, gas_limit).await,
        }
    }

    async fn eoa_call(&self, to: Address, data: Bytes, description: &str, gas_limit: u64) -> Result<()> {
        const MAX_RETRIES: usize = 3;
        let mut last_err = None;

        for attempt in 1..=MAX_RETRIES {
            let tx = TransactionRequest::default()
                .to(to)
                .input(alloy::rpc::types::TransactionInput::new(data.clone()))
                .gas_limit(gas_limit);

            match self.provider.send_transaction(tx).await {
                Ok(pending) => {
                    match pending.get_receipt().await {
                        Ok(receipt) => {
                            info!("CTF: {} confirmed (tx {})", description,
                                &receipt.transaction_hash.to_string()[..16]);
                            return Ok(());
                        }
                        Err(e) => {
                            last_err = Some(anyhow::anyhow!("tx wait error: {}", e));
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    last_err = Some(anyhow::anyhow!("{}", msg));
                    if attempt < MAX_RETRIES {
                        warn!("CTF: {} attempt {}/{} failed: {} — retrying...", description, attempt, MAX_RETRIES, msg);
                        sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Unknown error")))
    }

    async fn safe_call(&self, to: Address, data: Bytes, description: &str, gas_limit: u64) -> Result<()> {
        let proxy = self
            .config
            .proxy_wallet
            .as_deref()
            .unwrap_or("")
            .parse::<Address>()
            .context("Invalid proxy wallet address")?;

        const MAX_RETRIES: usize = 3;
        let mut gas_multiplier = 1u64;

        for attempt in 1..=MAX_RETRIES {
            let safe = IGnosisSafe::new(proxy, &self.provider);

            // Get current nonce
            let nonce = safe.nonce().call().await.context("Safe nonce failed")?._0;

            // Compute Safe tx hash
            let tx_hash = safe
                .getTransactionHash(
                    to,
                    U256::ZERO,
                    data.clone(),
                    0u8,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    Address::ZERO,
                    Address::ZERO,
                    nonce,
                )
                .call()
                .await
                .context("getTransactionHash failed")?
                ._0;

            // Sign with raw signing key (no EIP-191 prefix for Gnosis Safe v1.3.0)
            use alloy::signers::SignerSync;
            let sig = self.signer.sign_hash_sync(&tx_hash.into())?;
            let sig_bytes = sig.as_bytes();

            // Adjust v for Gnosis Safe (expects v = 27 or 28)
            let mut sig_vec = sig_bytes.to_vec();
            if sig_vec[64] < 27 {
                sig_vec[64] += 27;
            }

            let fee_data = self.provider.get_fee_history(1, alloy::eips::BlockNumberOrTag::Latest, &[]).await;
            let base_tip = 50_000_000_000u128; // 50 Gwei minimum
            let priority_fee = base_tip * gas_multiplier as u128;

            let tx = TransactionRequest::default()
                .to(proxy)
                .input(alloy::rpc::types::TransactionInput::new(
                    IGnosisSafe::execTransactionCall {
                        to,
                        value: U256::ZERO,
                        data: data.clone(),
                        operation: 0u8,
                        safeTxGas: U256::ZERO,
                        baseGas: U256::ZERO,
                        gasPrice: U256::ZERO,
                        gasToken: Address::ZERO,
                        refundReceiver: Address::ZERO,
                        signatures: Bytes::from(sig_vec),
                    }
                    .abi_encode()
                    .into(),
                ))
                .gas_limit(gas_limit + 50_000)
                .max_priority_fee_per_gas(priority_fee)
                .max_fee_per_gas(priority_fee * 2 + 100_000_000_000u128);

            match self.provider.send_transaction(tx).await {
                Ok(pending) => {
                    match pending.get_receipt().await {
                        Ok(receipt) => {
                            info!("CTF(Safe): {} confirmed", description);
                            return Ok(());
                        }
                        Err(e) => {
                            if attempt < MAX_RETRIES {
                                warn!("CTF(Safe): {} receipt error: {} — retrying...", description, e);
                                gas_multiplier *= 2;
                                sleep(Duration::from_secs(3)).await;
                            } else {
                                bail!("CTF(Safe): {} failed: {}", description, e);
                            }
                        }
                    }
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        warn!("CTF(Safe): {} send error (attempt {}): {} — retrying...", description, attempt, e);
                        gas_multiplier *= 2;
                        sleep(Duration::from_secs(3)).await;
                    } else {
                        bail!("CTF(Safe): {} failed: {}", description, e);
                    }
                }
            }
        }

        bail!("CTF: {} failed after {} attempts", description, MAX_RETRIES)
    }
}

// ── Standalone balance helpers ────────────────────────────────────────────────

/// Fetch USDC and native POL balances via a read-only RPC connection.
///
/// - `usdc_address` — wallet that holds USDC (proxy address in proxy mode, EOA otherwise)
/// - `pol_address`  — EOA that pays gas (always the private-key address)
///
/// Returns `(usdc_f64, pol_f64)` — USDC with 6-decimal precision,
/// POL formatted to 4 decimal places worth of precision.
pub async fn fetch_live_balances(
    rpc_url: &str,
    usdc_address: &str,
    pol_address: &str,
) -> anyhow::Result<(f64, f64)> {
    let usdc_addr: Address = usdc_address.parse().context("Invalid USDC wallet address")?;
    let pol_addr: Address  = pol_address.parse().context("Invalid POL wallet address")?;

    let provider = alloy::providers::ProviderBuilder::new()
        .on_http(rpc_url.parse().context("Invalid RPC URL")?);

    // USDC balance (ERC-20, 6 decimals) — from the USDC-holding wallet
    let usdc = IERC20::new(USDC_ADDRESS, &provider);
    let usdc_raw = usdc
        .balanceOf(usdc_addr)
        .call()
        .await
        .context("USDC balanceOf failed")?
        ._0;
    let usdc_f = usdc_raw.to::<u128>() as f64 / 1_000_000.0;

    // POL (native token, 18 decimals) — always from EOA (gas payer)
    let pol_wei = provider
        .get_balance(pol_addr)
        .await
        .context("get_balance (POL) failed")?;
    let pol_f = (pol_wei.to::<u128>() as f64 / 1e18 * 10000.0).round() / 10000.0;

    Ok((usdc_f, pol_f))
}
