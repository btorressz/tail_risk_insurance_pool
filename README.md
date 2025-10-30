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
