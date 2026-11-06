//! One fixed legacy exit. Every other vault remains deposit-only.
use crate::*;
use anchor_lang::solana_program::pubkey;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::Token;
use dex_pool::program::DexPool;

pub const RELEASE_AUTHORITY: Pubkey = pubkey!("FJF6YMp72V8NHUniRZxweeZd5CyyTNYY96dUKuaG2kmw");
pub const LEGACY_POOL: Pubkey = pubkey!("2A9fNRik2FogYNcnCJw6seYL7Xpjx6zRYAzcM7AZsxZA");
pub const LEGACY_LP: Pubkey = pubkey!("7rmNkUJg8uZwmPeyN29QdnGjPvc49FmsT5LwKTJSrF5N");
pub const LIKE: Pubkey = pubkey!("CJMihkPYswa3k6az9SUbepjKnkJQ6KWpGG5p9qW9n7NV");
pub const LIKER: Pubkey = pubkey!("8UqYNLpm5fzG8LZXVaje6nf6gEdvB6WeLY1rLzoAnoc6");

#[account]
#[derive(InitSpace)]
pub struct LegacyRelease {
    pub pool: Pubkey,
    pub recipient: Pubkey,
    pub lp_redeemed: u64,
    pub historical_locked: u64,
    pub like_received: u64,
    pub liker_received: u64,
    pub released_ts: i64,
}

#[derive(Accounts)]
pub struct ReleaseLegacyLiquidity<'info> {
    #[account(mut, address=RELEASE_AUTHORITY)]
    pub deployment: Signer<'info>,
    #[account(address=crate::ID, constraint=program.programdata_address()? == Some(program_data.key()))]
    pub program: Program<'info, crate::program::LpLocker>,
    #[account(constraint=program_data.upgrade_authority_address == Some(deployment.key()))]
    pub program_data: Box<Account<'info, ProgramData>>,
    #[account(init,payer=deployment,space=8+LegacyRelease::INIT_SPACE,seeds=[b"legacy-release"],bump)]
    pub release: Box<Account<'info, LegacyRelease>>,
    #[account(mut,address=LEGACY_POOL,has_one=base_mint,has_one=quote_mint,has_one=lp_mint,
        has_one=base_vault,has_one=quote_vault,constraint=pool.burn_bps == 0 @ LockerError::LegacyBurnEnabled)]
    pub pool: Box<Account<'info, dex_pool::Pool>>,
    #[account(address=LIKE)]
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut,address=LIKER)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut,address=LEGACY_LP)]
    pub lp_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut,seeds=[b"stats",LEGACY_LP.as_ref()],bump=stats.bump,constraint=stats.mint==LEGACY_LP @ LockerError::WrongLegacyAccount)]
    pub stats: Box<Account<'info, LockStats>>,
    #[account(seeds=[b"vault_authority"],bump)]
    /// CHECK: fixed signer PDA, only used for the fixed legacy redemption CPI.
    pub vault_authority: UncheckedAccount<'info>,
    #[account(mut,seeds=[b"vault",LEGACY_LP.as_ref()],bump,token::mint=lp_mint,token::authority=vault_authority,token::token_program=token_program)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,token::mint=base_mint,token::authority=pool,token::token_program=token_program)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,token::mint=quote_mint,token::authority=pool,token::token_program=token_program)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,token::mint=base_mint,token::authority=deployment,token::token_program=token_program)]
    pub deployment_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,token::mint=quote_mint,token::authority=deployment,token::token_program=token_program)]
    pub deployment_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(constraint=dex_program.programdata_address()? == Some(dex_program_data.key()),
        constraint=dex_program_data.upgrade_authority_address == Some(deployment.key()))]
    pub dex_program_data: Box<Account<'info, ProgramData>>,
    pub dex_program: Program<'info, DexPool>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn release(ctx: Context<ReleaseLegacyLiquidity>, min_like: u64, min_liker: u64) -> Result<()> {
    let a = ctx.accounts;
    for (actual,mint) in [(a.deployment_base.key(),LIKE),(a.deployment_quote.key(),LIKER)] {
        require_keys_eq!(actual,get_associated_token_address(&a.deployment.key(),&mint),LockerError::WrongLegacyAccount);
    }
    let lp=a.vault.amount;
    require!(lp>0,LockerError::ZeroAmount);
    let before=[a.deployment_base.amount,a.deployment_quote.amount,a.pool.total_lp];
    let signer:&[&[&[u8]]]=&[&[b"vault_authority",&[ctx.bumps.vault_authority]]];
    dex_pool::cpi::release_locked_legacy_liquidity(CpiContext::new_with_signer(a.dex_program.to_account_info(),
        dex_pool::cpi::accounts::ReleaseLockedLegacyLiquidity {
            deployment:a.deployment.to_account_info(),program:a.dex_program.to_account_info(),program_data:a.dex_program_data.to_account_info(),
            locker_authority:a.vault_authority.to_account_info(),pool:a.pool.to_account_info(),
            base_mint:a.base_mint.to_account_info(),quote_mint:a.quote_mint.to_account_info(),lp_mint:a.lp_mint.to_account_info(),
            base_vault:a.base_vault.to_account_info(),quote_vault:a.quote_vault.to_account_info(),locker_vault:a.vault.to_account_info(),
            deployment_base:a.deployment_base.to_account_info(),deployment_quote:a.deployment_quote.to_account_info(),
            token_program:a.token_program.to_account_info(),
        },signer),min_like,min_liker)?;
    a.vault.reload()?; a.pool.reload()?; a.deployment_base.reload()?; a.deployment_quote.reload()?;
    let like=a.deployment_base.amount.checked_sub(before[0]).ok_or(LockerError::MathOverflow)?;
    let liker=a.deployment_quote.amount.checked_sub(before[1]).ok_or(LockerError::MathOverflow)?;
    require!(a.vault.amount==0 && a.pool.total_lp.checked_add(lp)==Some(before[2])
        && like>=min_like && liker>=min_liker,LockerError::ReleasePostcondition);
    **a.release = LegacyRelease {
        pool: a.pool.key(),
        recipient: a.deployment.key(),
        lp_redeemed: lp,
        historical_locked: a.stats.total_locked.max(lp),
        like_received: like,
        liker_received: liker,
        released_ts: Clock::get()?.unix_timestamp,
    };
    a.stats.total_locked = 0; // receipts and LegacyRelease preserve historical inflows; this counter is custody.
    emit!(LegacyReleased {
        lp,
        like,
        liker,
        recipient: a.deployment.key()
    });
    Ok(())
}
#[event]
pub struct LegacyReleased {
    pub lp: u64,
    pub like: u64,
    pub liker: u64,
    pub recipient: Pubkey,
}
