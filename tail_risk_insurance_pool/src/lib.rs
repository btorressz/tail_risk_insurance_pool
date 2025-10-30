use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, Transfer},
};

declare_id!("9XjXYmL9TLB3FuszEuXCTkjC6a4vHZ5TPWczyNMLKHRg");

const SCALE: u128 = 1_000_000; // 1e6 fixed-point
const BPS_DENOM: u128 = 10_000;

// Storage bounds (tune for your needs)
const MAX_LOTS: usize = 16;
const MAX_ORACLES: usize = 16;

// ----------------------------- Program ------------------------------------

#[program]
pub mod tail_risk_insurance_pool {
    use super::*;

    // ----------------------------- admin/init -----------------------------

    pub fn initialize(ctx: Context<Initialize>, params: InitializeParams) -> Result<()> {
        let state = &mut ctx.accounts.state;

        // Basic wiring
        state.admin = ctx.accounts.admin.key();
        state.usdc_mint = ctx.accounts.usdc_mint.key();
        state.protocol_treasury = params.protocol_treasury;
        state.paused = false;
        state.processing = false;
        state.payout_policy = params.payout_policy as u8;

        // Fees / bounds
        state.protocol_fee_bps = params.protocol_fee_bps;
        state.referral_fee_bps = params.referral_fee_bps;

        // Limits / UX
        state.user_deposit_cap_fp = params.user_deposit_cap_fp;
        state.min_deposit_fp = params.min_deposit_fp;
        state.lockup_secs = params.lockup_secs;
        state.min_seconds_between_deposits = params.min_seconds_between_deposits;

        // Epoch policy
        state.epoch_cap_fp = params.epoch_cap_fp;
        state.rolling_mode = params.rolling_mode;
        state.max_stale_secs = params.max_stale_secs;

        // Severity curve (quadratic w/ floor)
        state.sev_quad_a_fp = params.sev_quad_a_fp;
        state.sev_quad_b_fp = params.sev_quad_b_fp;
        state.sev_quad_c_fp = params.sev_quad_c_fp;
        state.severity_floor_bps = params.severity_floor_bps;

        // Tranche weights
        state.tranche_weight_senior_bps = params.tranche_weight_senior_bps;
        state.tranche_weight_junior_bps = params.tranche_weight_junior_bps;

        // Accounting
        state.last_event_ts = 0;
        state.total_deposited_fp = 0;
        state.carryover_shortfall_fp = 0;

        state.bump = ctx.bumps.state;

        // Param sanity
        assert_param_bounds(state)?;

        emit!(Initialized { admin: state.admin, usdc_mint: state.usdc_mint });
        Ok(())
    }

    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.paused = paused;
        emit!(Paused { paused });
        Ok(())
    }

    pub fn set_policy(ctx: Context<AdminOnly>, payout_policy: u8, epoch_cap_fp: Option<u128>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.payout_policy = payout_policy;
        if let Some(cap) = epoch_cap_fp {
            state.epoch_cap_fp = cap;
        }
        assert_param_bounds(state)?;
        Ok(())
    }

    pub fn set_curve_and_weights(
        ctx: Context<AdminOnly>,
        sev_quad_a_fp: u128,
        sev_quad_b_fp: u128,
        sev_quad_c_fp: u128,
        severity_floor_bps: u16,
        tranche_weight_senior_bps: u16,
        tranche_weight_junior_bps: u16,
    ) -> Result<()> {
        let s = &mut ctx.accounts.state;
        s.sev_quad_a_fp = sev_quad_a_fp;
        s.sev_quad_b_fp = sev_quad_b_fp;
        s.sev_quad_c_fp = sev_quad_c_fp;
        s.severity_floor_bps = severity_floor_bps;
        s.tranche_weight_senior_bps = tranche_weight_senior_bps;
        s.tranche_weight_junior_bps = tranche_weight_junior_bps;
        assert_param_bounds(s)?;
        Ok(())
    }

    pub fn start_epoch(
        ctx: Context<StartEpoch>,
        epoch_id: u64,
        start_ts: i64,
        end_ts: i64, // may be 0 for "open/rolling" mode
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(start_ts <= now, ErrorCode::EpochNotActive);
        if end_ts != 0 {
            require!(end_ts > start_ts, ErrorCode::EpochNotActive);
        }

        let epoch = &mut ctx.accounts.epoch;
        epoch.epoch_id = epoch_id;
        epoch.start_ts = start_ts;
        epoch.end_ts = end_ts; // 0 = rolling/open ended
        epoch.total_stake_snapshot_fp = 0;
        epoch.total_payout_fp = 0;
        epoch.severity_bps = 0;
        epoch.user_cap_bps = 0;
        epoch.epoch_cap_fp = 0;
        epoch.shortfall_fp = 0;
        epoch.triggered = false;
        epoch.closed = false;
        epoch.evidence_hash = [0u8; 32];
        epoch.evidence_ts = 0;
        epoch.bump = ctx.bumps.epoch;

        emit!(EpochStarted { epoch_id, start_ts, end_ts });
        Ok(())
    }

    // ----------------------------- user flow -----------------------------

    /// Deposit into a chosen tranche (0 = senior, 1 = junior)
    pub fn deposit_insurance(
        ctx: Context<DepositInsurance>,
        amount_usdc: u64,
        tranche: u8,
        referrer_opt: Option<Pubkey>,
    ) -> Result<()> {
        // Snapshot read-only to avoid &mut during CPI
        let (paused, min_deposit_fp, user_cap_fp, proto_bps, ref_bps, min_cd_secs) = {
            let s = &ctx.accounts.state;
            (s.paused, s.min_deposit_fp, s.user_deposit_cap_fp, s.protocol_fee_bps, s.referral_fee_bps, s.min_seconds_between_deposits)
        };
        require!(!paused, ErrorCode::Paused);

        // Amount checks
        let amount_fp = to_fp_u64(amount_usdc)?;
        require!(amount_fp >= min_deposit_fp, ErrorCode::MinDeposit);

        let position = &mut ctx.accounts.position;

        // Rate limit deposits
        let now = Clock::get()?.unix_timestamp;
        if position.last_deposit_ts != 0 && min_cd_secs > 0 {
            require!(
                now.saturating_sub(position.last_deposit_ts) >= min_cd_secs,
                ErrorCode::DepositCooldown
            );
        }

        // Tranche routing
        require!(tranche <= 1, ErrorCode::Unauthorized);

        // Run transfer (user -> vault)
        transfer_tokens_user(
            &ctx.accounts.user_ata,
            &ctx.accounts.vault_ata,
            &ctx.accounts.user,
            &ctx.accounts.token_program,
            amount_usdc,
        )?;

        // Fees
        let proto_fee_fp = mul_div_floor_u128(amount_fp, proto_bps as u128, BPS_DENOM)?;
        let ref_fee_fp   = mul_div_floor_u128(amount_fp, ref_bps as u128, BPS_DENOM)?;
        let proto_fee_u64 = from_fp_to_u64(proto_fee_fp)?;
        let ref_fee_u64   = from_fp_to_u64(ref_fee_fp)?;

        // Protocol fee transfer (vault -> protocol_ata) via PDA signer
        if proto_fee_u64 > 0 {
            transfer_tokens_pda(
                &ctx.accounts.vault_ata,
                &ctx.accounts.protocol_treasury_ata,
                &ctx.accounts.state,
                &ctx.accounts.token_program,
                proto_fee_u64,
            )?;
            emit!(ProtocolFeeTaken { amount_u64: proto_fee_u64 });
        }

        // Referral fee transfer (optional)
        if let Some(refer) = referrer_opt {
            if refer != Pubkey::default() && ref_fee_u64 > 0 {
                let ref_ata = ctx.accounts.referrer_ata.as_ref().ok_or(ErrorCode::Unauthorized)?;
                require_keys_eq!(refer, ref_ata.owner, ErrorCode::Unauthorized);
                transfer_tokens_pda(
                    &ctx.accounts.vault_ata,
                    ref_ata,
                    &ctx.accounts.state,
                    &ctx.accounts.token_program,
                    ref_fee_u64,
                )?;
                emit!(ReferralFeeTaken { amount_u64: ref_fee_u64, referrer: refer });
                position.referrer = refer;
            }
        }

        // Net credit
        let net_fp = amount_fp.saturating_sub(proto_fee_fp.saturating_add(ref_fee_fp));
        position.owner = ctx.accounts.user.key();

        // Update lots (FIFO) for chosen tranche
        if tranche == 0 {
            push_lot(&mut position.senior_lots, Lot { amount_fp: net_fp, ts: now })?;
            position.senior_deposited_fp = position.senior_deposited_fp.saturating_add(net_fp);
            position.senior_withdrawable_fp = position.senior_withdrawable_fp.saturating_add(net_fp);
        } else {
            push_lot(&mut position.junior_lots, Lot { amount_fp: net_fp, ts: now })?;
            position.junior_deposited_fp = position.junior_deposited_fp.saturating_add(net_fp);
            position.junior_withdrawable_fp = position.junior_withdrawable_fp.saturating_add(net_fp);
        }

        position.last_deposit_ts = now;

        // Cap per-user (sum across tranches)
        let user_total = position.senior_deposited_fp.saturating_add(position.junior_deposited_fp);
        require!(user_total <= user_cap_fp, ErrorCode::UserCapExceeded);

        // Update pool accounting after CPIs
        let state = &mut ctx.accounts.state;
        state.total_deposited_fp = state.total_deposited_fp.saturating_add(net_fp);

        emit!(Deposited { owner: position.owner, amount_fp: net_fp, referrer: position.referrer, tranche });
        Ok(())
    }

    /// Withdraw from a selected tranche (0 senior, 1 junior)
    pub fn withdraw(ctx: Context<Withdraw>, amount_usdc: u64, tranche: u8) -> Result<()> {
        let state_chk = &ctx.accounts.state;
        require!(!state_chk.paused, ErrorCode::Paused);
        require!(tranche <= 1, ErrorCode::Unauthorized);

        let amount_fp = to_fp_u64(amount_usdc)?;
        let position = &mut ctx.accounts.position;

        // Consume matured lots per lockup_secs (FIFO)
        let now = Clock::get()?.unix_timestamp;
        let lockup = state_chk.lockup_secs;

        let mut remaining = amount_fp;

        if tranche == 0 {
            // senior — split borrows via helper to satisfy borrow checker
            let (lots_ref, withdrawable_ref) = senior_parts(position);
            mature_and_consume(lots_ref, lockup, now, withdrawable_ref, &mut remaining)?;
        } else {
            // junior — split borrows via helper to satisfy borrow checker
            let (lots_ref, withdrawable_ref) = junior_parts(position);
            mature_and_consume(lots_ref, lockup, now, withdrawable_ref, &mut remaining)?;
        }
        // Must have fully satisfied desired amount
        require!(remaining == 0, ErrorCode::InsufficientPoolBalance);

        // Bookkeeping: reduce deposited_fp and pool total
        if tranche == 0 {
            position.senior_deposited_fp = position.senior_deposited_fp.saturating_sub(amount_fp);
        } else {
            position.junior_deposited_fp = position.junior_deposited_fp.saturating_sub(amount_fp);
        }
        let state = &mut ctx.accounts.state;
        state.total_deposited_fp = state.total_deposited_fp.saturating_sub(amount_fp);

        // Transfer vault -> user
        transfer_tokens_pda(
            &ctx.accounts.vault_ata,
            &ctx.accounts.user_ata,
            &ctx.accounts.state,
            &ctx.accounts.token_program,
            amount_usdc,
        )?;

        emit!(Withdrawn { owner: position.owner, amount_fp, tranche });
        Ok(())
    }

    // ----------------------------- event / payout -----------------------------

    pub fn trigger_event(
        ctx: Context<TriggerEvent>,
        severity_input_bps: u16,               // input to curve
        user_cap_bps: Option<u16>,
        epoch_cap_fp_override: Option<u128>,
        evidence_hash: Option<[u8; 32]>,
        evidence_ts_opt: Option<i64>,          // if oracle data has timestamp
    ) -> Result<()> {
        // Oracle allowlist: either admin OR an allowed oracle
        {
            let signer = ctx.accounts.admin_or_oracle.key();
            let list = &ctx.accounts.oracle_list;
            require!(list.enabled, ErrorCode::Unauthorized);
            // If admin is also allowed implicitly:
            if signer != ctx.accounts.state.admin {
                require!(oracle_is_allowed(list, signer), ErrorCode::Unauthorized);
            }
        }

        let state = &mut ctx.accounts.state;
        let epoch = &mut ctx.accounts.epoch;
        let now = Clock::get()?.unix_timestamp;

        require!(!epoch.triggered, ErrorCode::EpochAlreadyTriggered);
        // Rolling mode allows end_ts == 0
        if epoch.end_ts != 0 {
            require!(now >= epoch.start_ts && now <= epoch.end_ts, ErrorCode::EpochNotActive);
        } else {
            require!(now >= epoch.start_ts, ErrorCode::EpochNotActive);
        }

        // Optional staleness check
        if let Some(e_ts) = evidence_ts_opt {
            if state.max_stale_secs > 0 {
                require!(now.saturating_sub(e_ts) <= state.max_stale_secs, ErrorCode::EpochNotActive);
            }
            epoch.evidence_ts = e_ts;
        }

        // Snapshot pool
        epoch.total_stake_snapshot_fp = state.total_deposited_fp;

        // Effective severity via curve + floor
        let sev_eff_bps = effective_severity_bps(
            severity_input_bps as u128,
            state.sev_quad_a_fp,
            state.sev_quad_b_fp,
            state.sev_quad_c_fp,
            state.severity_floor_bps,
        )?;
        epoch.severity_bps = sev_eff_bps as u16;
        epoch.user_cap_bps = user_cap_bps.unwrap_or(0);

        // Policy-cap on epoch liability
        if state.payout_policy == PayoutPolicy::EpochBounded as u8 {
            epoch.epoch_cap_fp = epoch_cap_fp_override.unwrap_or(state.epoch_cap_fp);
        } else {
            epoch.epoch_cap_fp = 0;
        }

        epoch.triggered = true;
        state.last_event_ts = now;

        // Freeze pool during claims
        state.paused = true;

        if let Some(h) = evidence_hash {
            epoch.evidence_hash = h;
        }

        emit!(EventTriggered {
            epoch_id: epoch.epoch_id,
            severity_bps: epoch.severity_bps,
            user_cap_bps: epoch.user_cap_bps,
            policy: state.payout_policy,
            evidence_hash: epoch.evidence_hash,
        });

        Ok(())
    }

    /// Per-user payout with claim receipt (prevents double claims)
    pub fn payout_user(ctx: Context<PayoutUser>) -> Result<()> {
        // Reentrancy-style guard
        {
            let s = &ctx.accounts.state;
            require!(!s.processing, ErrorCode::Busy);
        }
        {
            let s = &mut ctx.accounts.state;
            s.processing = true;
        }

        let res = (|| -> Result<()> {
            let state = &ctx.accounts.state;
            let epoch = &mut ctx.accounts.epoch;
            require!(epoch.triggered && !epoch.closed, ErrorCode::EpochNotActive);

            // Compute max epoch liability
            let pool_balance_fp = vault_balance_fp(&ctx.accounts.vault_ata)?;
            let policy = state.payout_policy;

            let base_liability_fp = {
                let sev_fp = epoch.severity_bps as u128;
                mul_div_floor_u128(epoch.total_stake_snapshot_fp, sev_fp, BPS_DENOM)?
            };

            // If underfunded, record shortfall (carryover)
            if base_liability_fp > pool_balance_fp {
                epoch.shortfall_fp = base_liability_fp.saturating_sub(pool_balance_fp);
            }

            // Policy cap
            let mut liability_cap_fp = base_liability_fp;
            if policy == PayoutPolicy::EpochBounded as u8 {
                liability_cap_fp = core::cmp::min(liability_cap_fp, epoch.epoch_cap_fp);
            }

            // Never exceed current pool USDC balance
            let max_liability_fp = core::cmp::min(liability_cap_fp, pool_balance_fp);
            require!(max_liability_fp > 0, ErrorCode::NothingToPayout);

            // User position (uses tranche-weighted stake)
            let position = &mut ctx.accounts.position;
            require_keys_eq!(position.owner, ctx.accounts.user.key(), ErrorCode::Unauthorized);

            // Weighted stake at snapshot approximated by current (pool is paused)
            let effective_user_stake_fp = weighted_stake_fp(
                position.senior_deposited_fp,
                position.junior_deposited_fp,
                state.tranche_weight_senior_bps as u128,
                state.tranche_weight_junior_bps as u128,
            )?;

            if effective_user_stake_fp == 0 || epoch.total_stake_snapshot_fp == 0 {
                return err!(ErrorCode::NothingToPayout);
            }

            // Pro-rata share
            let user_share_fp = mul_div_floor_u128(max_liability_fp, effective_user_stake_fp, epoch.total_stake_snapshot_fp)?;

            // Optional per-user cap (for Capped policy)
            let final_user_payout_fp = if policy == PayoutPolicy::Capped as u8 && epoch.user_cap_bps > 0 {
                let cap_fp = mul_div_floor_u128(effective_user_stake_fp, epoch.user_cap_bps as u128, BPS_DENOM)?;
                core::cmp::min(user_share_fp, cap_fp)
            } else {
                user_share_fp
            };

            // Check claim receipt (no double claim)
            let claim = &mut ctx.accounts.claim;
            require!(claim.claimed_fp == 0, ErrorCode::NothingToPayout);

            // Ensure room in epoch
            let remaining_epoch_room_fp = max_liability_fp.saturating_sub(epoch.total_payout_fp);
            let pay_fp = core::cmp::min(final_user_payout_fp, remaining_epoch_room_fp);
            require!(pay_fp > 0, ErrorCode::NothingToPayout);

            // Update epoch (accumulator)
            epoch.total_payout_fp = epoch.total_payout_fp.saturating_add(pay_fp);

            // Transfer vault -> user
            let pay_u64 = from_fp_to_u64(pay_fp)?;
            transfer_tokens_pda(
                &ctx.accounts.vault_ata,
                &ctx.accounts.user_ata,
                &ctx.accounts.state,
                &ctx.accounts.token_program,
                pay_u64,
            )?;

            // Write receipt
            claim.epoch_id = epoch.epoch_id;
            claim.owner = ctx.accounts.user.key();
            claim.claimed_fp = pay_fp;
            claim.bump = ctx.bumps.claim;

            emit!(UserPayout {
                epoch_id: epoch.epoch_id,
                owner: position.owner,
                payout_fp: pay_fp
            });
            Ok(())
        })();

        // Always clear the guard
        let s = &mut ctx.accounts.state;
        s.processing = false;
        res
    }

    /// Finalize an epoch, unpause the pool, optionally sweep dust to treasury.
    pub fn finalize_epoch(ctx: Context<FinalizeEpoch>, sweep_dust_u64: Option<u64>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let epoch = &mut ctx.accounts.epoch;

        require!(epoch.triggered && !epoch.closed, ErrorCode::EpochNotActive);

        // Record shortfall to state for future make-good accounting
        if epoch.shortfall_fp > 0 {
            state.carryover_shortfall_fp = state.carryover_shortfall_fp.saturating_add(epoch.shortfall_fp);
        }

        // Optional dust sweep (any spare above total_deposited_fp is interpreted as fees/excess)
        if let Some(sweep) = sweep_dust_u64 {
            if sweep > 0 {
                let pool_bal_fp = vault_balance_fp(&ctx.accounts.vault_ata)?;
                let principal_fp = state.total_deposited_fp;
                if pool_bal_fp > principal_fp {
                    let dust_fp = pool_bal_fp.saturating_sub(principal_fp);
                    let move_u64 = core::cmp::min(sweep, from_fp_to_u64(dust_fp)?);
                    if move_u64 > 0 {
                        transfer_tokens_pda(
                            &ctx.accounts.vault_ata,
                            &ctx.accounts.protocol_treasury_ata,
                            state,
                            &ctx.accounts.token_program,
                            move_u64,
                        )?;
                    }
                }
            }
        }

        epoch.closed = true;
        state.paused = false;

        emit!(EpochFinalized { epoch_id: epoch.epoch_id });

        Ok(())
    }

    // ----------------------------- views (no state change) -----------------------------

    pub fn pool_stats(ctx: Context<ViewPoolStats>) -> Result<PoolStats> {
        let s = &ctx.accounts.state;
        let bal = vault_balance_fp(&ctx.accounts.vault_ata)?;
        Ok(PoolStats {
            total_deposited_fp: s.total_deposited_fp,
            pool_balance_fp: bal,
            payout_policy: s.payout_policy,
            epoch_cap_fp: s.epoch_cap_fp,
            carryover_shortfall_fp: s.carryover_shortfall_fp,
            rolling_mode: s.rolling_mode,
        })
    }

    pub fn user_position_view(ctx: Context<ViewUserPosition>) -> Result<UserPositionView> {
        let p = &ctx.accounts.position;
        Ok(UserPositionView {
            owner: p.owner,
            senior_deposited_fp: p.senior_deposited_fp,
            junior_deposited_fp: p.junior_deposited_fp,
            senior_withdrawable_fp: p.senior_withdrawable_fp,
            junior_withdrawable_fp: p.junior_withdrawable_fp,
            last_deposit_ts: p.last_deposit_ts,
            referrer: p.referrer,
        })
    }

    pub fn epoch_stats(ctx: Context<ViewEpochStats>) -> Result<EpochStats> {
        let e = &ctx.accounts.epoch;
        Ok(EpochStats {
            epoch_id: e.epoch_id,
            start_ts: e.start_ts,
            end_ts: e.end_ts,
            total_stake_snapshot_fp: e.total_stake_snapshot_fp,
            total_payout_fp: e.total_payout_fp,
            severity_bps: e.severity_bps,
            user_cap_bps: e.user_cap_bps,
            epoch_cap_fp: e.epoch_cap_fp,
            shortfall_fp: e.shortfall_fp,
            triggered: e.triggered,
            closed: e.closed,
            evidence_hash: e.evidence_hash,
            evidence_ts: e.evidence_ts,
        })
    }

    pub fn quote_user_payout(ctx: Context<QuoteUserPayout>) -> Result<QuoteOut> {
        let s = &ctx.accounts.state;
        let e = &ctx.accounts.epoch;
        let p = &ctx.accounts.position;

        require!(e.triggered, ErrorCode::EpochNotActive);

        let bal = vault_balance_fp(&ctx.accounts.vault_ata)?;
        let base_liab = mul_div_floor_u128(e.total_stake_snapshot_fp, e.severity_bps as u128, BPS_DENOM)?;
        let liab_cap = if s.payout_policy == PayoutPolicy::EpochBounded as u8 {
            core::cmp::min(base_liab, e.epoch_cap_fp)
        } else {
            base_liab
        };
        let max_liab = core::cmp::min(liab_cap, bal);

        let eff_user = weighted_stake_fp(
            p.senior_deposited_fp,
            p.junior_deposited_fp,
            s.tranche_weight_senior_bps as u128,
            s.tranche_weight_junior_bps as u128,
        )?;

        let mut user_share = 0u128;
        if e.total_stake_snapshot_fp > 0 && eff_user > 0 {
            user_share = mul_div_floor_u128(max_liab, eff_user, e.total_stake_snapshot_fp)?;
            if s.payout_policy == PayoutPolicy::Capped as u8 && e.user_cap_bps > 0 {
                let cap = mul_div_floor_u128(eff_user, e.user_cap_bps as u128, BPS_DENOM)?;
                user_share = core::cmp::min(user_share, cap);
            }
        }

        Ok(QuoteOut { user_payout_fp: user_share, max_epoch_liability_fp: max_liab })
    }

    pub fn quote_deposit(ctx: Context<ViewPoolStats>, amount_usdc: u64) -> Result<DepositQuote> {
        let s = &ctx.accounts.state;
        let fp = to_fp_u64(amount_usdc)?;
        let pf = mul_div_floor_u128(fp, s.protocol_fee_bps as u128, BPS_DENOM)?;
        let rf = mul_div_floor_u128(fp, s.referral_fee_bps as u128, BPS_DENOM)?;
        let net = fp.saturating_sub(pf.saturating_add(rf));
        Ok(DepositQuote {
            net_fp: net,
            protocol_fee_u64: from_fp_to_u64(pf)?,
            referral_fee_u64: from_fp_to_u64(rf)?,
        })
    }

    pub fn quote_withdraw(ctx: Context<ViewUserPosition>, amount_usdc: u64, tranche: u8) -> Result<WithdrawQuote> {
        require!(tranche <= 1, ErrorCode::Unauthorized);
        let p = &ctx.accounts.position;
        let need_fp = to_fp_u64(amount_usdc)?;
        let avail_fp = if tranche == 0 { p.senior_withdrawable_fp } else { p.junior_withdrawable_fp };
        Ok(WithdrawQuote {
            can_withdraw: avail_fp >= need_fp,
            available_fp: avail_fp,
            requested_fp: need_fp,
        })
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    pub usdc_mint: Account<'info, Mint>,

    /// Program state PDA
    #[account(
        init,
        payer = admin,
        seeds = [b"state", crate::id().as_ref()],
        bump,
        space = 8 + State::SIZE
    )]
    pub state: Account<'info, State>,

    /// Program-owned vault ATA (authority = state)
    #[account(
        init,
        payer = admin,
        associated_token::mint = usdc_mint,
        associated_token::authority = state
    )]
    pub vault_ata: Account<'info, TokenAccount>,

    /// Oracle allowlist (enabled by default, admin populates later)
    #[account(
        init,
        payer = admin,
        seeds = [b"oracle", crate::id().as_ref()],
        bump,
        space = 8 + OracleList::SIZE
    )]
    pub oracle_list: Account<'info, OracleList>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"state", crate::id().as_ref()],
        bump = state.bump,
        has_one = admin @ ErrorCode::Unauthorized
    )]
    pub state: Account<'info, State>,
}

#[derive(Accounts)]
#[instruction(epoch_id: u64)]
pub struct StartEpoch<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"state", crate::id().as_ref()],
        bump = state.bump,
        has_one = admin @ ErrorCode::Unauthorized
    )]
    pub state: Account<'info, State>,

    #[account(
        init,
        payer = admin,
        seeds = [b"epoch", epoch_id.to_le_bytes().as_ref()],
        bump,
        space = 8 + Epoch::SIZE
    )]
    pub epoch: Account<'info, Epoch>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DepositInsurance<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    pub usdc_mint: Account<'info, Mint>,

    #[account(mut, seeds = [b"state", crate::id().as_ref()], bump = state.bump)]
    pub state: Account<'info, State>,

    /// Program-owned vault
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = state
    )]
    pub vault_ata: Account<'info, TokenAccount>,

    /// User's USDC ATA (source)
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = user
    )]
    pub user_ata: Account<'info, TokenAccount>,

    /// Treasury ATA (destination for protocol fee)
    #[account(mut)]
    pub protocol_treasury_ata: Account<'info, TokenAccount>,

    /// Optional: Referrer ATA
    #[account(mut)]
    pub referrer_ata: Option<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = user,
        seeds = [b"position", user.key().as_ref()],
        bump,
        space = 8 + UserPosition::SIZE
    )]
    pub position: Account<'info, UserPosition>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    pub usdc_mint: Account<'info, Mint>,

    #[account(seeds = [b"state", crate::id().as_ref()], bump = state.bump)]
    pub state: Account<'info, State>,

    /// Program-owned vault
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = state
    )]
    pub vault_ata: Account<'info, TokenAccount>,

    /// User ATA (destination)
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = user
    )]
    pub user_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"position", user.key().as_ref()],
        bump = position.bump,
        constraint = position.owner == user.key() @ ErrorCode::Unauthorized
    )]
    pub position: Account<'info, UserPosition>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct TriggerEvent<'info> {
    #[account(mut)]
    pub admin_or_oracle: Signer<'info>,

    #[account(mut, seeds = [b"state", crate::id().as_ref()], bump = state.bump)]
    pub state: Account<'info, State>,

    #[account(
        mut,
        seeds = [b"epoch", epoch.epoch_id.to_le_bytes().as_ref()],
        bump = epoch.bump
    )]
    pub epoch: Account<'info, Epoch>,

    #[account(seeds = [b"oracle", crate::id().as_ref()], bump = oracle_list.bump)]
    pub oracle_list: Account<'info, OracleList>,
}

#[derive(Accounts)]
pub struct PayoutUser<'info> {
    #[account(mut)]
    pub user: Signer<'info>, // payer for claim
    pub usdc_mint: Account<'info, Mint>,

    #[account(seeds = [b"state", crate::id().as_ref()], bump = state.bump)]
    pub state: Account<'info, State>,

    #[account(
        mut,
        seeds = [b"epoch", epoch.epoch_id.to_le_bytes().as_ref()],
        bump = epoch.bump
    )]
    pub epoch: Account<'info, Epoch>,

    /// Program-owned vault
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = state
    )]
    pub vault_ata: Account<'info, TokenAccount>,

    /// User ATA (destination)
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = user
    )]
    pub user_ata: Account<'info, TokenAccount>,

    #[account(mut, seeds = [b"position", user.key().as_ref()], bump = position.bump)]
    pub position: Account<'info, UserPosition>,

    #[account(
        init_if_needed,
        payer = user,
        seeds = [b"claim", epoch.epoch_id.to_le_bytes().as_ref(), user.key().as_ref()],
        bump,
        space = 8 + ClaimReceipt::SIZE
    )]
    pub claim: Account<'info, ClaimReceipt>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeEpoch<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"state", crate::id().as_ref()],
        bump = state.bump,
        has_one = admin @ ErrorCode::Unauthorized
    )]
    pub state: Account<'info, State>,

    #[account(
        mut,
        seeds = [b"epoch", epoch.epoch_id.to_le_bytes().as_ref()],
        bump = epoch.bump
    )]
    pub epoch: Account<'info, Epoch>,

    /// Program-owned vault
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = state
    )]
    pub vault_ata: Account<'info, TokenAccount>,

    /// Treasury ATA for dust sweep
    #[account(mut)]
    pub protocol_treasury_ata: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

// ----------------------------- view contexts -----------------------------

#[derive(Accounts)]
pub struct ViewPoolStats<'info> {
    pub state: Account<'info, State>,
    #[account(associated_token::mint = usdc_mint, associated_token::authority = state)]
    pub vault_ata: Account<'info, TokenAccount>,
    pub usdc_mint: Account<'info, Mint>,
}

#[derive(Accounts)]
pub struct ViewUserPosition<'info> {
    #[account(seeds = [b"position", position.owner.as_ref()], bump = position.bump)]
    pub position: Account<'info, UserPosition>,
}

#[derive(Accounts)]
pub struct ViewEpochStats<'info> {
    pub epoch: Account<'info, Epoch>,
}

#[derive(Accounts)]
pub struct QuoteUserPayout<'info> {
    pub state: Account<'info, State>,
    pub epoch: Account<'info, Epoch>,
    #[account(associated_token::mint = usdc_mint, associated_token::authority = state)]
    pub vault_ata: Account<'info, TokenAccount>,
    pub usdc_mint: Account<'info, Mint>,
    pub position: Account<'info, UserPosition>,
}

// ---------------------------------------------------------------------------
// State & Enums
// ---------------------------------------------------------------------------

#[repr(u8)]
pub enum PayoutPolicy {
    Proportional = 0,
    Capped = 1,
    EpochBounded = 2,
}

#[account]
pub struct State {
    pub admin: Pubkey,
    pub usdc_mint: Pubkey,
    pub protocol_treasury: Pubkey,

    pub paused: bool,
    pub processing: bool, // reentrancy-style guard
    pub payout_policy: u8, // 0=Proportional,1=Capped,2=EpochBounded

    // Fees / limits
    pub user_deposit_cap_fp: u128,
    pub min_deposit_fp: u128,
    pub protocol_fee_bps: u16,
    pub referral_fee_bps: u16,
    pub lockup_secs: i64,
    pub min_seconds_between_deposits: i64,

    // Epoch policy
    pub epoch_cap_fp: u128,
    pub rolling_mode: bool,
    pub max_stale_secs: i64,

    // Severity curve
    pub sev_quad_a_fp: u128, // coefficients in fixed-point SCALE
    pub sev_quad_b_fp: u128,
    pub sev_quad_c_fp: u128,
    pub severity_floor_bps: u16,

    // Tranche weights
    pub tranche_weight_senior_bps: u16,
    pub tranche_weight_junior_bps: u16,

    // Accounting
    pub last_event_ts: i64,
    pub total_deposited_fp: u128,
    pub carryover_shortfall_fp: u128,

    pub bump: u8,
}
impl State {
    pub const SIZE: usize =
        32 + 32 + 32 +
        1 + 1 + 1 +
        16 + 16 + 2 + 2 + 8 + 8 +
        16 + 1 + 8 +
        16 + 16 + 16 + 2 +
        2 + 2 +
        8 + 16 + 16 +
        1;
}

#[account]
pub struct UserPosition {
    pub owner: Pubkey,
    // Tranche balances
    pub senior_deposited_fp: u128,
    pub junior_deposited_fp: u128,
    pub senior_withdrawable_fp: u128,
    pub junior_withdrawable_fp: u128,
    // FIFO lots per tranche
    pub senior_lots: Lots,
    pub junior_lots: Lots,

    pub last_deposit_ts: i64,
    pub referrer: Pubkey,
    pub bump: u8,
}
impl UserPosition {
    pub const SIZE: usize =
        32 + 16 + 16 + 16 + 16 +
        Lots::SIZE + Lots::SIZE +
        8 + 32 + 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default)]
pub struct Lot {
    pub amount_fp: u128,
    pub ts: i64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct Lots {
    pub head: u8, // ring buffer
    pub len: u8,
    pub buf: [Lot; MAX_LOTS],
}
impl Lots {
    pub const SIZE: usize = 1 + 1 + (MAX_LOTS * (16 + 8));
}

#[account]
pub struct Epoch {
    pub epoch_id: u64,
    pub start_ts: i64,
    pub end_ts: i64, // 0 = open/rolling
    pub total_stake_snapshot_fp: u128,
    pub total_payout_fp: u128,
    pub shortfall_fp: u128,

    // Policy params (effective)
    pub severity_bps: u16,
    pub user_cap_bps: u16,   // for Capped
    pub epoch_cap_fp: u128,  // for EpochBounded

    // Lifecycle
    pub triggered: bool,
    pub closed: bool,

    // Evidence
    pub evidence_hash: [u8; 32],
    pub evidence_ts: i64,

    pub bump: u8,
}
impl Epoch {
    pub const SIZE: usize =
        8 + 8 + 8 + 16 + 16 + 16 +
        2 + 2 + 16 +
        1 + 1 +
        32 + 8 +
        1;
}

#[account]
pub struct ClaimReceipt {
    pub epoch_id: u64,
    pub owner: Pubkey,
    pub claimed_fp: u128,
    pub bump: u8,
}
impl ClaimReceipt {
    pub const SIZE: usize = 8 + 32 + 16 + 1;
}

#[account]
pub struct OracleList {
    pub enabled: bool,
    pub count: u8,
    pub keys: [Pubkey; MAX_ORACLES],
    pub bump: u8,
}
impl OracleList {
    pub const SIZE: usize = 1 + 1 + (MAX_ORACLES * 32) + 1;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct Initialized { pub admin: Pubkey, pub usdc_mint: Pubkey }

#[event]
pub struct Deposited { pub owner: Pubkey, pub amount_fp: u128, pub referrer: Pubkey, pub tranche: u8 }

#[event]
pub struct ProtocolFeeTaken { pub amount_u64: u64 }

#[event]
pub struct ReferralFeeTaken { pub amount_u64: u64, pub referrer: Pubkey }

#[event]
pub struct EpochStarted { pub epoch_id: u64, pub start_ts: i64, pub end_ts: i64 }

#[event]
pub struct EventTriggered {
    pub epoch_id: u64,
    pub severity_bps: u16,
    pub user_cap_bps: u16,
    pub policy: u8,
    pub evidence_hash: [u8; 32],
}

#[event]
pub struct UserPayout { pub epoch_id: u64, pub owner: Pubkey, pub payout_fp: u128 }

#[event]
pub struct Withdrawn { pub owner: Pubkey, pub amount_fp: u128, pub tranche: u8 }

#[event]
pub struct EpochFinalized { pub epoch_id: u64 }

#[event]
pub struct Paused { pub paused: bool }

// ---------------------------------------------------------------------------
// Return types for view/quote
// ---------------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct PoolStats {
    pub total_deposited_fp: u128,
    pub pool_balance_fp: u128,
    pub payout_policy: u8,
    pub epoch_cap_fp: u128,
    pub carryover_shortfall_fp: u128,
    pub rolling_mode: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UserPositionView {
    pub owner: Pubkey,
    pub senior_deposited_fp: u128,
    pub junior_deposited_fp: u128,
    pub senior_withdrawable_fp: u128,
    pub junior_withdrawable_fp: u128,
    pub last_deposit_ts: i64,
    pub referrer: Pubkey,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct EpochStats {
    pub epoch_id: u64,
    pub start_ts: i64,
    pub end_ts: i64,
    pub total_stake_snapshot_fp: u128,
    pub total_payout_fp: u128,
    pub severity_bps: u16,
    pub user_cap_bps: u16,
    pub epoch_cap_fp: u128,
    pub shortfall_fp: u128,
    pub triggered: bool,
    pub closed: bool,
    pub evidence_hash: [u8; 32],
    pub evidence_ts: i64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct QuoteOut {
    pub user_payout_fp: u128,
    pub max_epoch_liability_fp: u128,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct DepositQuote {
    pub net_fp: u128,
    pub protocol_fee_u64: u64,
    pub referral_fee_u64: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct WithdrawQuote {
    pub can_withdraw: bool,
    pub available_fp: u128,
    pub requested_fp: u128,
}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitializeParams {
    pub protocol_treasury: Pubkey,
    pub payout_policy: u8,
    pub user_deposit_cap_fp: u128,
    pub min_deposit_fp: u128,
    pub protocol_fee_bps: u16,
    pub referral_fee_bps: u16,
    pub lockup_secs: i64,
    pub min_seconds_between_deposits: i64,

    pub epoch_cap_fp: u128,
    pub rolling_mode: bool,
    pub max_stale_secs: i64,

    pub sev_quad_a_fp: u128,
    pub sev_quad_b_fp: u128,
    pub sev_quad_c_fp: u128,
    pub severity_floor_bps: u16,

    pub tranche_weight_senior_bps: u16,
    pub tranche_weight_junior_bps: u16,
}

// ---------------------------------------------------------------------------
// Helpers & Math
// ---------------------------------------------------------------------------

fn to_fp_u64(amount_u64: u64) -> Result<u128> {
    let a = amount_u64 as u128;
    a.checked_mul(SCALE).ok_or_else(|| error!(ErrorCode::MathOverflow))
}

fn from_fp_to_u64(amount_fp: u128) -> Result<u64> {
    let x = amount_fp
        .checked_div(SCALE)
        .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    u64::try_from(x).map_err(|_| error!(ErrorCode::MathOverflow))
}

fn mul_div_floor_u128(a: u128, b: u128, denom: u128) -> Result<u128> {
    let num = a.checked_mul(b).ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    num.checked_div(denom).ok_or_else(|| error!(ErrorCode::MathOverflow))
}

fn vault_balance_fp(vault: &Account<TokenAccount>) -> Result<u128> {
    let bal = vault.amount as u128;
    bal.checked_mul(SCALE).ok_or_else(|| error!(ErrorCode::MathOverflow))
}

fn weighted_stake_fp(senior_fp: u128, junior_fp: u128, w_senior_bps: u128, w_junior_bps: u128) -> Result<u128> {
    let s_w = mul_div_floor_u128(senior_fp, w_senior_bps, BPS_DENOM)?;
    let j_w = mul_div_floor_u128(junior_fp, w_junior_bps, BPS_DENOM)?;
    s_w.checked_add(j_w).ok_or_else(|| error!(ErrorCode::MathOverflow))
}

// Quadratic severity: a*x^2 + b*x + c (x in bps), coefficients in SCALE
fn effective_severity_bps(x_bps: u128, a_fp: u128, b_fp: u128, c_fp: u128, floor_bps: u16) -> Result<u128> {
    // Convert x_bps to fixed-point
    let x_fp = x_bps.checked_mul(SCALE).ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    let x2 = mul_div_floor_u128(x_fp, x_fp, SCALE)?; // x^2 in SCALE
    let ax2 = mul_div_floor_u128(a_fp, x2, SCALE)?;
    let bx  = mul_div_floor_u128(b_fp, x_fp, SCALE)?;
    let sum = ax2.checked_add(bx).ok_or_else(|| error!(ErrorCode::MathOverflow))?
                 .checked_add(c_fp).ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    // Convert back to bps (divide by SCALE)
    let bps = sum.checked_div(SCALE).ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    let floored = core::cmp::max(bps, floor_bps as u128);
    Ok(core::cmp::min(floored, BPS_DENOM)) // clamp to 10000 bps
}

// Lots helpers
fn push_lot(lots: &mut Lots, lot: Lot) -> Result<()> {
    if (lots.len as usize) < MAX_LOTS {
        let idx = ((lots.head as usize) + (lots.len as usize)) % MAX_LOTS;
        lots.buf[idx] = lot;
        lots.len += 1;
        Ok(())
    } else {
        // simple back-pressure: reject if ring full
        err!(ErrorCode::TooManyLots)
    }
}

fn pop_matured(lots: &mut Lots, lockup_secs: i64, now: i64) -> Option<Lot> {
    if lots.len == 0 { return None; }
    let lot = lots.buf[lots.head as usize];
    if now.saturating_sub(lot.ts) >= lockup_secs {
        lots.head = ((lots.head as usize + 1) % MAX_LOTS) as u8;
        lots.len -= 1;
        Some(lot)
    } else {
        None
    }
}

fn mature_and_consume(
    lots: &mut Lots,
    lockup_secs: i64,
    now: i64,
    withdrawable_fp: &mut u128,
    remaining: &mut u128,
) -> Result<()> {
    // Make withdrawable by maturing lots
    loop {
        if let Some(l) = pop_matured(lots, lockup_secs, now) {
            *withdrawable_fp = withdrawable_fp.saturating_add(l.amount_fp);
        } else { break; }
    }
    // Consume withdrawable toward target
    let take = core::cmp::min(*withdrawable_fp, *remaining);
    *withdrawable_fp = withdrawable_fp.saturating_sub(take);
    *remaining = remaining.saturating_sub(take);
    Ok(())
}

// ---- split helpers to avoid double mutable borrows on the same struct ----
fn senior_parts(p: &mut UserPosition) -> (&mut Lots, &mut u128) {
    (&mut p.senior_lots, &mut p.senior_withdrawable_fp)
}
fn junior_parts(p: &mut UserPosition) -> (&mut Lots, &mut u128) {
    (&mut p.junior_lots, &mut p.junior_withdrawable_fp)
}

// Oracle helpers
fn oracle_is_allowed(list: &OracleList, signer: Pubkey) -> bool {
    for i in 0..(list.count as usize) {
        if list.keys[i] == signer { return true; }
    }
    false
}

// Param guards
fn assert_param_bounds(s: &State) -> Result<()> {
    require!(s.protocol_fee_bps as u32 <= 1_000, ErrorCode::ParamOutOfBounds);
    require!(s.referral_fee_bps as u32 <= 1_000, ErrorCode::ParamOutOfBounds);
    require!((s.tranche_weight_senior_bps as u32) <= 10_000, ErrorCode::ParamOutOfBounds);
    require!((s.tranche_weight_junior_bps as u32) <= 10_000, ErrorCode::ParamOutOfBounds);
    Ok(())
}

// user authority (Signer) transfer
fn transfer_tokens_user<'info>(
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    user: &Signer<'info>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let cpi_ctx = CpiContext::new(
        token_program.to_account_info(),
        Transfer {
            from: from.to_account_info(),
            to: to.to_account_info(),
            authority: user.to_account_info(),
        },
    );
    token::transfer(cpi_ctx, amount)
}

// state PDA authority transfer
fn transfer_tokens_pda<'info>(
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    state: &Account<'info, State>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let program_id_bytes = crate::id();
    let seeds: &[&[u8]] = &[
        b"state",
        program_id_bytes.as_ref(),
        &[state.bump],
    ];
    let signer = &[seeds];

    let cpi_ctx = CpiContext::new_with_signer(
        token_program.to_account_info(),
        Transfer {
            from: from.to_account_info(),
            to: to.to_account_info(),
            authority: state.to_account_info(),
        },
        signer,
    );
    token::transfer(cpi_ctx, amount)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum ErrorCode {
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Protocol is paused")]
    Paused,
    #[msg("Deposit below minimum")]
    MinDeposit,
    #[msg("User cap exceeded")]
    UserCapExceeded,
    #[msg("Lockup not expired or lots not matured")]
    LockupNotExpired,
    #[msg("Epoch not active / invalid timing")]
    EpochNotActive,
    #[msg("Epoch already triggered")]
    EpochAlreadyTriggered,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Insufficient pool balance or withdrawable funds")]
    InsufficientPoolBalance,
    #[msg("Nothing to payout or already claimed")]
    NothingToPayout,
    #[msg("Deposit cooldown in effect")]
    DepositCooldown,
    #[msg("Too many deposit lots (try consolidating)")]
    TooManyLots,
    #[msg("Params out of allowed bounds")]
    ParamOutOfBounds,
    #[msg("Operation busy (reentrancy guard)")]
    Busy,
}
