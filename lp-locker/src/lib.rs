//! Deposit-only vaults, except a co-signed one-shot exit for the fixed legacy DEX LP.
//! Historical receipts remain records, not withdrawal claims.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

mod legacy;
pub use legacy::*;

declare_id!("9c18DLHnXrQC6MAVHpig9kpAE8YX7mZYhB6LtVT84RW1");

#[program]
pub mod lp_locker {
    use super::*;

    pub fn release_legacy_liquidity(ctx: Context<ReleaseLegacyLiquidity>, min_like_out: u64, min_liker_out: u64) -> Result<()> { legacy::release(ctx, min_like_out, min_liker_out) }

    /// Create the vault for `mint`: a token account at PDA `[b"vault", mint]` whose authority is
    /// the PDA `[b"vault_authority"]`.
    ///
    /// Only the fixed legacy LP has an explicit, co-signed migration exit.
    pub fn init_vault(ctx: Context<InitVault>) -> Result<()> {
        let stats = &mut ctx.accounts.stats;
        stats.mint = ctx.accounts.mint.key();
        stats.total_locked = 0;
        stats.lock_count = 0;
        stats.bump = ctx.bumps.stats;
        emit!(VaultCreatedEvent {
            mint: stats.mint,
            vault: ctx.accounts.vault.key(),
        });
        Ok(())
    }

    /// Lock `amount`; the retired legacy mint cannot receive new lock deposits.
    ///
    /// The caller transfers from their own token account into the vault. It is their own property
    /// being given up and nobody else is affected, so this needs no permission check beyond the
    /// caller signing for the source account.
    ///
    /// The amount credited is the **actual balance delta of the vault**, not `amount`. For a
    /// classic SPL mint these are identical; for a Token-2022 mint carrying a transfer fee the
    /// vault receives less than was sent, and recording the requested figure would overstate what
    /// is provably locked. Measuring the delta keeps `total_locked` equal to what the vault really
    /// holds, which is the number a verifier will independently check against the chain.
    pub fn lock(ctx: Context<Lock>, amount: u64) -> Result<()> {
        require!(amount > 0, LockerError::ZeroAmount);
        require_keys_neq!(ctx.accounts.mint.key(), LEGACY_LP, LockerError::WrongLegacyAccount);

        let before = ctx.accounts.vault.amount;
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.depositor_token.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        ctx.accounts.vault.reload()?;
        let locked = ctx
            .accounts
            .vault
            .amount
            .checked_sub(before)
            .ok_or(LockerError::MathOverflow)?;
        // A transfer that moved nothing must not be recorded as a lock, nor leave a receipt behind
        // claiming one happened.
        require!(locked > 0, LockerError::ZeroAmount);

        let stats = &mut ctx.accounts.stats;
        stats.total_locked = stats
            .total_locked
            .checked_add(locked)
            .ok_or(LockerError::MathOverflow)?;
        stats.lock_count = stats
            .lock_count
            .checked_add(1)
            .ok_or(LockerError::MathOverflow)?;

        // The receipt is cumulative: locking twice from the same address adds to one account rather
        // than needing a fresh one each time. It is a record for explorers and front-ends — it
        // confers no claim on the tokens, because there is nothing to claim them with.
        let receipt = &mut ctx.accounts.receipt;
        if receipt.depositor == Pubkey::default() {
            receipt.depositor = ctx.accounts.depositor.key();
            receipt.mint = ctx.accounts.mint.key();
            receipt.first_locked_ts = Clock::get()?.unix_timestamp;
            receipt.bump = ctx.bumps.receipt;
        }
        receipt.amount = receipt
            .amount
            .checked_add(locked)
            .ok_or(LockerError::MathOverflow)?;
        receipt.last_locked_ts = Clock::get()?.unix_timestamp;

        emit!(LockedEvent {
            mint: ctx.accounts.mint.key(),
            depositor: ctx.accounts.depositor.key(),
            amount: locked,
            requested: amount,
            depositor_total: receipt.amount,
            mint_total: stats.total_locked,
            ts: receipt.last_locked_ts,
        });
        Ok(())
    }

    /// Permissionless crank: reconcile `stats.total_locked` up to the vault's actual balance.
    ///
    /// Tokens can reach a vault two ways: through `lock` (which records a receipt and updates stats),
    /// or by a plain token transfer straight to the vault address. The second kind is locked just as
    /// in custody (the fixed legacy mint has its separate one-shot exit), but it leaves `total_locked` understating
    /// what is really locked. This instruction closes that gap so the counter stays a number people
    /// can trust, without anyone needing permission to fix it.
    ///
    /// `donate` uses exactly that second path: it transfers LP directly rather than CPI-ing `lock`,
    /// because adding `lock`'s extra accounts to its distribution instruction would overflow a
    /// 4 KB SBF stack frame that has already caused a mainnet outage once. So this crank is what
    /// keeps donate's continuously-locked LP visible in the on-chain total.
    ///
    /// Moves no tokens and takes no authority — it only copies a balance the chain already agrees on
    /// into the stats account.
    pub fn sync_stats(ctx: Context<SyncStats>) -> Result<()> {
        let actual = ctx.accounts.vault.amount;
        let stats = &mut ctx.accounts.stats;
        // Apart from the fixed legacy exit, vaults cannot shrink. That exit resets this
        // custody counter atomically while retaining the historical migration record.
        require!(actual >= stats.total_locked, LockerError::VaultShrank);
        let added = actual - stats.total_locked;
        stats.total_locked = actual;
        emit!(StatsSyncedEvent {
            mint: stats.mint,
            added,
            total_locked: actual,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Per-mint totals. Exists so a verifier can read one account to get "how much of this mint is
/// permanently locked" instead of summing events. Purely informational: the legacy one-shot exit has a separate migration record.
#[account]
#[derive(InitSpace)]
pub struct LockStats {
    pub mint: Pubkey,
    /// Current recorded custody, reset only by the fixed legacy exit. Should equal the vault's balance;
    /// a direct transfer into the vault (which anyone can do, bypassing `lock`) would make the
    /// vault larger than this figure. That is harmless — such tokens are locked just as hard, they
    /// simply have no receipt.
    pub total_locked: u64,
    pub lock_count: u64,
    pub bump: u8,
}

/// Per-depositor receipt, one per (mint, depositor), cumulative across locks.
/// A record of what someone gave up — deliberately **not** a claim ticket.
#[account]
#[derive(InitSpace)]
pub struct LockReceipt {
    pub depositor: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub first_locked_ts: i64,
    pub last_locked_ts: i64,
    pub bump: u8,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitVault<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    pub mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init,
        payer = payer,
        space = 8 + LockStats::INIT_SPACE,
        seeds = [b"stats", mint.key().as_ref()],
        bump,
    )]
    pub stats: Box<Account<'info, LockStats>>,

    /// CHECK: fixed authority PDA. This initializer never signs; only the fixed legacy
    /// exit can sign for this PDA, with both frozen authorities and the canonical mint.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = payer,
        seeds = [b"vault", mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = vault_authority,
        token::token_program = token_program,
    )]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Lock<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    pub mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [b"stats", mint.key().as_ref()],
        bump = stats.bump,
        has_one = mint,
    )]
    pub stats: Box<Account<'info, LockStats>>,

    /// `init_if_needed` is correct and safe here: the receipt holds no privileges, so a re-init
    /// grants nothing. The handler distinguishes first-time from subsequent locks by checking
    /// whether `depositor` is still zero, and only ever *adds* to `amount`.
    #[account(
        init_if_needed,
        payer = depositor,
        space = 8 + LockReceipt::INIT_SPACE,
        seeds = [b"receipt", mint.key().as_ref(), depositor.key().as_ref()],
        bump,
    )]
    pub receipt: Box<Account<'info, LockReceipt>>,

    /// Bound by seeds + `token::authority`, so the caller cannot redirect the deposit to an account
    /// they control and mint themselves a receipt for tokens that were never locked.
    #[account(
        mut,
        seeds = [b"vault", mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = vault_authority,
    )]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: never read, written, or signed for; only used to bind `vault`'s expected authority.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, token::mint = mint, token::authority = depositor)]
    pub depositor_token: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Permissionless — no signer at all. Reads the vault, writes only the stats counter.
#[derive(Accounts)]
pub struct SyncStats<'info> {
    #[account(
        mut,
        seeds = [b"stats", vault.mint.as_ref()],
        bump = stats.bump,
    )]
    pub stats: Box<Account<'info, LockStats>>,

    #[account(
        seeds = [b"vault", vault.mint.as_ref()],
        bump,
    )]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct VaultCreatedEvent {
    pub mint: Pubkey,
    pub vault: Pubkey,
}

#[event]
pub struct LockedEvent {
    pub mint: Pubkey,
    pub depositor: Pubkey,
    /// What the vault actually received (the credited figure).
    pub amount: u64,
    /// What the caller asked to send. Differs from `amount` only for a transfer-fee mint.
    pub requested: u64,
    pub depositor_total: u64,
    pub mint_total: u64,
    pub ts: i64,
}

#[event]
pub struct StatsSyncedEvent {
    pub mint: Pubkey,
    /// How much directly-transferred (receipt-less) balance this sync absorbed.
    pub added: u64,
    pub total_locked: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum LockerError {
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Vault balance is below the recorded total — impossible unless tokens left the vault")]
    VaultShrank,
    #[msg("Only the fixed legacy mint/pool and canonical recipient accounts are allowed")]
    WrongLegacyAccount,
    #[msg("Disable legacy periodic burn before redemption")]
    LegacyBurnEnabled,
    #[msg("Atomic redemption did not preserve intermediate balances or deliver assets")]
    ReleasePostcondition,
}
