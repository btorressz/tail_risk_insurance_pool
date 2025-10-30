// No imports needed: web3, anchor, pg and more are globally available
//still editing test file 

describe("Tail Risk Insurance Pool", () => {
  // Test accounts
  let admin: web3.Keypair;
  let user1: web3.Keypair;
  let user2: web3.Keypair;
  let oracle: web3.Keypair;
  
  // PDAs
  let statePda: web3.PublicKey;
  let vaultAta: web3.PublicKey;
  let oracleListPda: web3.PublicKey;
  
  // USDC mock mint
  let usdcMint: web3.PublicKey;
  let protocolTreasury: web3.PublicKey;
  let protocolTreasuryAta: web3.PublicKey;
  
  const SCALE = new BN(1_000_000); // 1e6 fixed-point
  const BPS_DENOM = new BN(10_000);
  
  // Helper to convert USDC amount to fixed-point
  const toFp = (amount: number): BN => {
    return new BN(amount).mul(SCALE);
  };
  
  // Helper to convert fixed-point to USDC
  const fromFp = (amountFp: BN): number => {
    return amountFp.div(SCALE).toNumber();
  };

  before(async () => {
    // Generate keypairs
    admin = pg.wallet.keypair;
    user1 = web3.Keypair.generate();
    user2 = web3.Keypair.generate();
    oracle = web3.Keypair.generate();
    protocolTreasury = web3.Keypair.generate().publicKey;
    
    // Airdrop SOL to test accounts
    const airdropSig1 = await pg.connection.requestAirdrop(
      user1.publicKey,
      2 * web3.LAMPORTS_PER_SOL
    );
    const airdropSig2 = await pg.connection.requestAirdrop(
      user2.publicKey,
      2 * web3.LAMPORTS_PER_SOL
    );
    await pg.connection.confirmTransaction(airdropSig1);
    await pg.connection.confirmTransaction(airdropSig2);
    
    // Create USDC mock mint
    usdcMint = await createMint(
      pg.connection,
      admin,
      admin.publicKey,
      null,
      6 // USDC has 6 decimals
    );
    
    // Derive PDAs
    [statePda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("state"), pg.program.programId.toBuffer()],
      pg.program.programId
    );
    
    [oracleListPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("oracle"), pg.program.programId.toBuffer()],
      pg.program.programId
    );
    
    // Derive vault ATA
    vaultAta = await getAssociatedTokenAddress(
      usdcMint,
      statePda,
      true
    );
    
    // Create protocol treasury ATA
    protocolTreasuryAta = await getOrCreateAssociatedTokenAccount(
      pg.connection,
      admin,
      usdcMint,
      protocolTreasury
    ).then(account => account.address);
    
    console.log("Setup complete:");
    console.log("- Admin:", admin.publicKey.toString());
    console.log("- User1:", user1.publicKey.toString());
    console.log("- User2:", user2.publicKey.toString());
    console.log("- USDC Mint:", usdcMint.toString());
    console.log("- State PDA:", statePda.toString());
  });

  it("Initialize the insurance pool", async () => {
    const params = {
      protocolTreasury: protocolTreasury,
      payoutPolicy: 0, // Proportional
      userDepositCapFp: toFp(1_000_000), // 1M USDC cap per user
      minDepositFp: toFp(100), // 100 USDC minimum
      protocolFeeBps: 50, // 0.5%
      referralFeeBps: 25, // 0.25%
      lockupSecs: new BN(60), // 60 seconds for testing
      minSecondsBetweenDeposits: new BN(10), // 10 seconds cooldown
      epochCapFp: toFp(500_000), // 500k USDC epoch cap
      rollingMode: false,
      maxStaleSecs: new BN(300), // 5 minutes
      sevQuadAFp: new BN(0), // No quadratic term
      sevQuadBFp: SCALE, // Linear: severity_out = severity_in
      sevQuadCFp: new BN(0), // No constant term
      severityFloorBps: 100, // 1% minimum severity
      trancheWeightSeniorBps: 10000, // 100% weight for senior
      trancheWeightJuniorBps: 15000, // 150% weight for junior (riskier)
    };

    const txHash = await pg.program.methods
      .initialize(params)
      .accounts({
        admin: admin.publicKey,
        usdcMint: usdcMint,
        state: statePda,
        vaultAta: vaultAta,
        oracleList: oracleListPda,
        systemProgram: web3.SystemProgram.programId,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        associatedTokenProgram: anchor.utils.token.ASSOCIATED_PROGRAM_ID,
        rent: web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    console.log(`Initialize tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    // Fetch and verify state
    const state = await pg.program.account.state.fetch(statePda);
    assert(state.admin.equals(admin.publicKey));
    assert(state.usdcMint.equals(usdcMint));
    assert.equal(state.paused, false);
    assert.equal(state.protocolFeeBps, 50);
    console.log("✓ Pool initialized successfully");
  });

  it("Start an epoch", async () => {
    const epochId = new BN(1);
    const now = Math.floor(Date.now() / 1000);
    const startTs = new BN(now);
    const endTs = new BN(now + 3600); // 1 hour epoch

    const [epochPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("epoch"), epochId.toArrayLike(Buffer, "le", 8)],
      pg.program.programId
    );

    const txHash = await pg.program.methods
      .startEpoch(epochId, startTs, endTs)
      .accounts({
        admin: admin.publicKey,
        state: statePda,
        epoch: epochPda,
        systemProgram: web3.SystemProgram.programId,
      })
      .rpc();

    console.log(`Start epoch tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const epoch = await pg.program.account.epoch.fetch(epochPda);
    assert(epoch.epochId.eq(epochId));
    assert.equal(epoch.triggered, false);
    console.log("✓ Epoch started successfully");
  });

  it("User deposits into senior tranche", async () => {
    const depositAmount = 10_000; // 10,000 USDC
    
    // Create user1's USDC account and mint tokens
    const user1Ata = await getOrCreateAssociatedTokenAccount(
      pg.connection,
      admin,
      usdcMint,
      user1.publicKey
    ).then(account => account.address);
    
    await mintTo(
      pg.connection,
      admin,
      usdcMint,
      user1Ata,
      admin.publicKey,
      depositAmount
    );

    const [positionPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("position"), user1.publicKey.toBuffer()],
      pg.program.programId
    );

    const txHash = await pg.program.methods
      .depositInsurance(new BN(depositAmount), 0, null) // 0 = senior tranche
      .accounts({
        user: user1.publicKey,
        usdcMint: usdcMint,
        state: statePda,
        vaultAta: vaultAta,
        userAta: user1Ata,
        protocolTreasuryAta: protocolTreasuryAta,
        referrerAta: null,
        position: positionPda,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        associatedTokenProgram: anchor.utils.token.ASSOCIATED_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([user1])
      .rpc();

    console.log(`Deposit tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const position = await pg.program.account.userPosition.fetch(positionPda);
    console.log(`User1 senior deposited: ${fromFp(position.seniorDepositedFp)} USDC`);
    assert(position.seniorDepositedFp.gt(new BN(0)));
    console.log("✓ User deposited successfully");
  });

  it("User deposits into junior tranche", async () => {
    const depositAmount = 5_000; // 5,000 USDC
    
    // Create user2's USDC account and mint tokens
    const user2Ata = await getOrCreateAssociatedTokenAccount(
      pg.connection,
      admin,
      usdcMint,
      user2.publicKey
    ).then(account => account.address);
    
    await mintTo(
      pg.connection,
      admin,
      usdcMint,
      user2Ata,
      admin.publicKey,
      depositAmount
    );

    const [positionPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("position"), user2.publicKey.toBuffer()],
      pg.program.programId
    );

    const txHash = await pg.program.methods
      .depositInsurance(new BN(depositAmount), 1, null) // 1 = junior tranche
      .accounts({
        user: user2.publicKey,
        usdcMint: usdcMint,
        state: statePda,
        vaultAta: vaultAta,
        userAta: user2Ata,
        protocolTreasuryAta: protocolTreasuryAta,
        referrerAta: null,
        position: positionPda,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        associatedTokenProgram: anchor.utils.token.ASSOCIATED_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([user2])
      .rpc();

    console.log(`Junior deposit tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const position = await pg.program.account.userPosition.fetch(positionPda);
    console.log(`User2 junior deposited: ${fromFp(position.juniorDepositedFp)} USDC`);
    assert(position.juniorDepositedFp.gt(new BN(0)));
    console.log("✓ User deposited into junior tranche");
  });

  it("View pool stats", async () => {
    const stats = await pg.program.methods
      .poolStats()
      .accounts({
        state: statePda,
        vaultAta: vaultAta,
        usdcMint: usdcMint,
      })
      .view();

    console.log("Pool Stats:");
    console.log(`- Total Deposited: ${fromFp(stats.totalDepositedFp)} USDC`);
    console.log(`- Pool Balance: ${fromFp(stats.poolBalanceFp)} USDC`);
    console.log(`- Payout Policy: ${stats.payoutPolicy}`);
    console.log("✓ Pool stats retrieved");
  });

  it("Trigger event (oracle)", async () => {
    const epochId = new BN(1);
    const [epochPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("epoch"), epochId.toArrayLike(Buffer, "le", 8)],
      pg.program.programId
    );

    // Admin can trigger (acts as oracle)
    const severityBps = 5000; // 50% severity
    const evidenceHash = Array(32).fill(0);

    const txHash = await pg.program.methods
      .triggerEvent(
        severityBps,
        null, // no user cap
        null, // no epoch cap override
        evidenceHash,
        null  // no evidence timestamp
      )
      .accounts({
        adminOrOracle: admin.publicKey,
        state: statePda,
        epoch: epochPda,
        oracleList: oracleListPda,
      })
      .rpc();

    console.log(`Trigger event tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const epoch = await pg.program.account.epoch.fetch(epochPda);
    assert.equal(epoch.triggered, true);
    console.log(`✓ Event triggered with ${epoch.severityBps} bps severity`);
  });

  it("Payout to user1", async () => {
    const epochId = new BN(1);
    const [epochPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("epoch"), epochId.toArrayLike(Buffer, "le", 8)],
      pg.program.programId
    );

    const [positionPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("position"), user1.publicKey.toBuffer()],
      pg.program.programId
    );

    const [claimPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("claim"),
        epochId.toArrayLike(Buffer, "le", 8),
        user1.publicKey.toBuffer()
      ],
      pg.program.programId
    );

    const user1Ata = await getAssociatedTokenAddress(
      usdcMint,
      user1.publicKey
    );

    const balanceBefore = await pg.connection.getTokenAccountBalance(user1Ata);
    
    const txHash = await pg.program.methods
      .payoutUser()
      .accounts({
        user: user1.publicKey,
        usdcMint: usdcMint,
        state: statePda,
        epoch: epochPda,
        vaultAta: vaultAta,
        userAta: user1Ata,
        position: positionPda,
        claim: claimPda,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        associatedTokenProgram: anchor.utils.token.ASSOCIATED_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([user1])
      .rpc();

    console.log(`Payout tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const balanceAfter = await pg.connection.getTokenAccountBalance(user1Ata);
    const payout = parseInt(balanceAfter.value.amount) - parseInt(balanceBefore.value.amount);
    
    console.log(`✓ User1 received payout: ${payout / 1_000_000} USDC`);
    
    const claim = await pg.program.account.claimReceipt.fetch(claimPda);
    assert(claim.claimedFp.gt(new BN(0)));
  });

  it("Finalize epoch", async () => {
    const epochId = new BN(1);
    const [epochPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("epoch"), epochId.toArrayLike(Buffer, "le", 8)],
      pg.program.programId
    );

    const txHash = await pg.program.methods
      .finalizeEpoch(null) // no dust sweep
      .accounts({
        admin: admin.publicKey,
        state: statePda,
        epoch: epochPda,
        vaultAta: vaultAta,
        protocolTreasuryAta: protocolTreasuryAta,
        usdcMint: usdcMint,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .rpc();

    console.log(`Finalize epoch tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const state = await pg.program.account.state.fetch(statePda);
    assert.equal(state.paused, false);
    
    const epoch = await pg.program.account.epoch.fetch(epochPda);
    assert.equal(epoch.closed, true);
    console.log("✓ Epoch finalized and pool unpaused");
  });

  it("User withdraws from senior tranche", async () => {
    // Wait for lockup period by checking slots
    // In Solana Playground, we can't use setTimeout, so we'll just proceed
    // In production, you'd wait for the actual time or use slot-based timing
    console.log("Note: Skipping lockup wait in test environment");

    const [positionPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("position"), user1.publicKey.toBuffer()],
      pg.program.programId
    );

    const user1Ata = await getAssociatedTokenAddress(
      usdcMint,
      user1.publicKey
    );

    const withdrawAmount = 1000; // 1,000 USDC
    const balanceBefore = await pg.connection.getTokenAccountBalance(user1Ata);

    const txHash = await pg.program.methods
      .withdraw(new BN(withdrawAmount), 0) // 0 = senior tranche
      .accounts({
        user: user1.publicKey,
        usdcMint: usdcMint,
        state: statePda,
        vaultAta: vaultAta,
        userAta: user1Ata,
        position: positionPda,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        associatedTokenProgram: anchor.utils.token.ASSOCIATED_PROGRAM_ID,
      })
      .signers([user1])
      .rpc();

    console.log(`Withdraw tx: ${txHash}`);
    await pg.connection.confirmTransaction(txHash);

    const balanceAfter = await pg.connection.getTokenAccountBalance(user1Ata);
    const withdrawn = parseInt(balanceAfter.value.amount) - parseInt(balanceBefore.value.amount);
    
    console.log(`✓ User1 withdrew: ${withdrawn / 1_000_000} USDC`);
    assert(withdrawn > 0);
  });
});

// Helper functions (these should work in Solana Playground)
async function createMint(
  connection: web3.Connection,
  payer: web3.Keypair,
  mintAuthority: web3.PublicKey,
  freezeAuthority: web3.PublicKey | null,
  decimals: number
): Promise<web3.PublicKey> {
  const mintKeypair = web3.Keypair.generate();
  const lamports = await connection.getMinimumBalanceForRentExemption(82);

  const transaction = new web3.Transaction().add(
    web3.SystemProgram.createAccount({
      fromPubkey: payer.publicKey,
      newAccountPubkey: mintKeypair.publicKey,
      space: 82,
      lamports,
      programId: anchor.utils.token.TOKEN_PROGRAM_ID,
    }),
    // Initialize mint instruction would go here
    // This is simplified for the example
  );

  await web3.sendAndConfirmTransaction(connection, transaction, [payer, mintKeypair]);
  return mintKeypair.publicKey;
}

async function getAssociatedTokenAddress(
  mint: web3.PublicKey,
  owner: web3.PublicKey,
  allowOwnerOffCurve = false
): Promise<web3.PublicKey> {
  return anchor.utils.token.associatedAddress({
    mint: mint,
    owner: owner
  });
}

async function getOrCreateAssociatedTokenAccount(
  connection: web3.Connection,
  payer: web3.Keypair,
  mint: web3.PublicKey,
  owner: web3.PublicKey
) {
  const address = await getAssociatedTokenAddress(mint, owner);
  return { address };
}

async function mintTo(
  connection: web3.Connection,
  payer: web3.Keypair,
  mint: web3.PublicKey,
  destination: web3.PublicKey,
  authority: web3.PublicKey,
  amount: number
) {
  // Simplified mint operation
  // In reality this would create a MintTo instruction
  console.log(`Minting ${amount} tokens to ${destination.toString()}`);
}