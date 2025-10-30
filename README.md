# tail_risk_insurance_pool

# 🛡️ Tail Risk Insurance Pool - Solana Program

A Solana-based **parametric insurance protocol** that provides automated tail-risk coverage through a **tranched liquidity pool system**.  
Users deposit **USDC** to provide coverage, and **claims are automatically paid out** when specified risk events are triggered by **authorized oracles**.

---

## 📄 Overview

This protocol implements a **decentralized insurance pool** where:

- **Liquidity Providers** deposit USDC into **senior** or **junior** tranches to earn fees.
- **Risk Events** are triggered by **authorized oracles** with severity measurements.
- **Automated Payouts** are calculated based on configurable **severity curves** and **tranche weightings**.
- **Epoch-based Coverage** allows time-bounded or rolling coverage periods.

---


## 🔑 Key Features

### 🏦 Dual-Tranche System
- **Senior Tranche**: Lower risk exposure, weighted protection
- **Junior Tranche**: Higher risk exposure, weighted protection
- Configurable **tranche weights** determine risk distribution

### 📊 Flexible Payout Policies
- `Proportional`: Pro-rata distribution based on stake
- `Capped`: Per-user caps on maximum payout
- `EpochBounded`: Total epoch liability caps

### 📈 Quadratic Severity Curve
- Configurable curve: `a*x² + b*x + c`
- Severity floor ensures **minimum payouts**
- Translates **oracle input** into payout percentage

### 🔒 Security Features
- **FIFO lockup** periods for withdrawals
- **Deposit cooldowns** to prevent gaming
- **Reentrancy guards** on critical operations
- **Oracle allowlist** for event triggering
- **Claim receipts** to prevent double-claiming

### 💰 Fee Structure
- **Protocol fees** on deposits
- Optional **referral fees**
- **Dust sweeping** to treasury on epoch finalization

  __


  ## 🧮 Core Concepts

### Fixed-Point Math
- All amounts use `1e6` fixed-point arithmetic  
  (`SCALE = 1_000_000`)  
  Ensures precise calculations without floating-point math

### Epochs
Coverage periods with defined parameters:
- Start / End timestamps (or rolling)
- Severity measurements from oracles
- Snapshot-based stake tracking
- **Evidence hash** for audit trails

### Lots
- Deposits are tracked as **FIFO lots** with timestamps
- Enables lockup enforcement and **mature withdrawal logic**

---


## 🧾 Program Instructions

### 🔧 Admin Operations

#### `initialize`
Initialize the protocol with global parameters:
- Treasury address
- Fee rates (protocol & referral)
- Deposit caps and minimums
- Lockup periods
- Severity curve coefficients
- Tranche weights

#### `set_paused`
- Emergency pause/unpause deposits and withdrawals

#### `set_policy`
- Update payout policy and epoch cap

#### `set_curve_and_weights`
- Adjust severity curve parameters and tranche weightings

#### `start_epoch`
- Create a new coverage epoch with time bounds

#### `finalize_epoch`
- Close an epoch, unpause the pool, and optionally **sweep dust fees** to treasury

  
### 'deposit_insurance'
- **Deposit USDC into chosen tranche (senior=0, junior=1):**

- Enforces minimum deposit amounts
- Applies protocol and referral fees
- Creates FIFO lot with timestamp
- Checks per-user deposit cap

###  'withdraw'
- **Withdraw from a tranche after lockup period:**

- Consumes matured FIFO lots
- Enforces lockup requirements
- Returns USDC to user

###  'Oracle Operations'
- trigger_event
- Authorized oracle triggers a covered event:

- Provides severity input (BPS)
- Applies severity curve transformation
- Snapshots pool state
- Pauses pool for claims processing
- Records evidence hash and timestamp
