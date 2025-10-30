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

---
