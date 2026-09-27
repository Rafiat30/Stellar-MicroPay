#![no_std]

/**
 * contracts/stellar-micropay-contract/src/lib.rs
 *
 * Stellar MicroPay — Soroban Smart Contract
 *
 * Provides:
 *   - Escrow payments (ROADMAP v2.1)
 *   - Creator tipping (ROADMAP v1.4)
 *   - Micro-transaction batching (ROADMAP v2.0)
 *   - NFT payment receipts (ROADMAP v1.5)
 *
 * Build:
 *   cargo build --target wasm32-unknown-unknown --release
 *
 * Deploy (Stellar CLI):
 *   stellar contract deploy \
 *     --wasm target/wasm32-unknown-unknown/release/stellar_micropay_contract.wasm \
 *     --source YOUR_SECRET_KEY \
 *     --network testnet
 */

use soroban_sdk::{
    contract, contractimpl, contracttype,
    token, Address, Env, Symbol,
};

// ─── Data types ───────────────────────────────────────────────────────────────

/// A single tip event recorded on-chain.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TipRecord {
    /// The sender's Stellar address
    pub from: Address,
    /// The recipient's Stellar address
    pub to: Address,
    /// Amount in stroops (1 XLM = 10_000_000 stroops)
    pub amount: i128,
    /// Ledger number when this tip was sent
    pub ledger: u32,
}

/// On-chain receipt metadata minted as proof of payment.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ReceiptMetadata {
    /// The payer's Stellar address
    pub from: Address,
    /// The payee's Stellar address
    pub to: Address,
    /// Amount in stroops (1 XLM = 10_000_000 stroops)
    pub amount: i128,
    /// ISO-8601 timestamp of when the receipt was minted
    pub timestamp: u64,
    /// Optional payment memo
    pub memo: Symbol,
    /// Ledger number when this receipt was minted
    pub ledger: u32,
}

/// Storage key for per-recipient tip totals
#[contracttype]
pub enum DataKey {
    Admin,
    TipTotal(Address),
    TipCount(Address),
    /// Latest tip record for a recipient (indexed by recipient + count)
    TipRecord(Address, u32),
    /// Total receipt count for a payer
    ReceiptCount(Address),
    /// Receipt record indexed by (payer, index)
    ReceiptRecord(Address, u32),
    /// Number of payment streams created so far (id counter)
    StreamCount,
    /// Payment stream record indexed by stream id
    Stream(u32),
}

/// A streaming payment channel escrowing funds from a payer to a recipient.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Stream {
    /// Address funding the stream (can top up and close; transferable)
    pub payer: Address,
    /// Address allowed to claim streamed funds
    pub recipient: Address,
    /// Token contract escrowed by this stream
    pub token: Address,
    /// Amount streamed per ledger (in stroops)
    pub rate_per_ledger: i128,
    /// Total amount deposited so far (in stroops)
    pub deposited: i128,
    /// Total amount claimed so far (in stroops)
    pub claimed: i128,
    /// Ledger number when the stream was opened
    pub start_ledger: u32,
}

/// Compute `(claimable, refundable)` for a stream at `current_ledger`.
///
/// - `claimable`: unclaimed streamed amount, capped by the unclaimed deposit
/// - `refundable`: deposit not yet streamed (paid back on close)
fn stream_progress(stream: &Stream, current_ledger: u32) -> (i128, i128) {
    let elapsed = (current_ledger.saturating_sub(stream.start_ledger)) as i128;
    // Cap the multiplication instead of panicking: an overflowing total can
    // only mean "everything has been streamed".
    let total_streamed = stream
        .rate_per_ledger
        .checked_mul(elapsed)
        .unwrap_or(i128::MAX);
    let claimable = total_streamed
        .saturating_sub(stream.claimed)
        .min(stream.deposited.saturating_sub(stream.claimed))
        .max(0);
    let refundable = stream
        .deposited
        .saturating_sub(total_streamed.max(stream.claimed))
        .max(0);
    (claimable, refundable)
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct MicroPayContract;

#[contractimpl]
impl MicroPayContract {

    // ─── Initialization ──────────────────────────────────────────────────────

    /// Initialize the contract with an admin address.
    /// Can only be called once.
    pub fn initialize(env: Env, admin: Address) {
        // Ensure not already initialized
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("Contract already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    // ─── Tipping ─────────────────────────────────────────────────────────────

    /// Send a tip from `from` to `to` using a Stellar token.
    ///
    /// Parameters:
    ///   - token_address: The SAC (Stellar Asset Contract) address for the token (e.g. XLM)
    ///   - from:          The sender (must authorize this call)
    ///   - to:            The recipient
    ///   - amount:        Amount in the token's smallest unit (stroops for XLM)
    ///
    /// This records the tip on-chain for analytics and emits an event.
    pub fn send_tip(
        env: Env,
        token_address: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) {
        // Require sender authorization
        from.require_auth();

        // Validate amount
        if amount <= 0 {
            panic!("Tip amount must be positive");
        }

        // Transfer tokens via the Stellar token interface (SAC)
        let token = token::Client::new(&env, &token_address);
        token.transfer(&from, &to, &amount);

        // Update on-chain tip totals for the recipient
        let current_total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TipTotal(to.clone()))
            .unwrap_or(0);

        let current_count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TipCount(to.clone()))
            .unwrap_or(0);

        env.storage()
            .instance()
            .set(&DataKey::TipTotal(to.clone()), &(current_total + amount));

        env.storage()
            .instance()
            .set(&DataKey::TipCount(to.clone()), &(current_count + 1));

        // Store the tip record so it can be queried later
        let record = TipRecord {
            from: from.clone(),
            to: to.clone(),
            amount,
            ledger: env.ledger().sequence(),
        };
        env.storage()
            .instance()
            .set(&DataKey::TipRecord(to.clone(), current_count), &record);

        // Emit an event for indexers
        env.events().publish(
            (Symbol::new(&env, "tip"), from, to.clone()),
            amount,
        );
    }

    // ─── Getters ─────────────────────────────────────────────────────────────

    /// Get the total amount tipped to a recipient (in stroops).
    pub fn get_tip_total(env: Env, recipient: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TipTotal(recipient))
            .unwrap_or(0)
    }

    /// Get the number of tips received by a recipient.
    pub fn get_tip_count(env: Env, recipient: Address) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TipCount(recipient))
            .unwrap_or(0)
    }

    /// Get the contract admin address.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Contract not initialized")
    }

    /// Get a specific tip record for a recipient by index.
    pub fn get_tip_record(env: Env, recipient: Address, index: u32) -> TipRecord {
        env.storage()
            .instance()
            .get(&DataKey::TipRecord(recipient, index))
            .expect("Tip record not found")
    }

    // ─── NFT Receipts ───────────────────────────────────────────────────────

    /// Mint an on-chain receipt as proof of payment.
    ///
    /// Stores receipt metadata (amount, timestamp, memo) under the payer's
    /// address and emits a `receipt` event. The returned `u32` is the receipt
    /// index (NFT ID) for this payer.
    ///
    /// Parameters:
    ///   - from:   The payer (must authorize this call)
    ///   - to:     The payee
    ///   - amount: Amount in stroops
    ///   - memo:   Optional payment memo (max 28 chars, passed as a Symbol)
    pub fn mint_receipt(
        env: Env,
        from: Address,
        to: Address,
        amount: i128,
        memo: Symbol,
    ) -> u32 {
        from.require_auth();

        if amount <= 0 {
            panic!("Receipt amount must be positive");
        }

        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ReceiptCount(from.clone()))
            .unwrap_or(0);

        let receipt = ReceiptMetadata {
            from: from.clone(),
            to,
            amount,
            timestamp: env.ledger().timestamp(),
            memo,
            ledger: env.ledger().sequence(),
        };

        env.storage()
            .instance()
            .set(&DataKey::ReceiptRecord(from.clone(), count), &receipt);

        env.storage()
            .instance()
            .set(&DataKey::ReceiptCount(from.clone()), &(count + 1));

        env.events().publish(
            (Symbol::new(&env, "receipt"), from),
            count,
        );

        count
    }

    /// Get the total number of receipts minted for a payer.
    pub fn get_receipt_count(env: Env, payer: Address) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ReceiptCount(payer))
            .unwrap_or(0)
    }

    /// Get a specific receipt for a payer by index.
    pub fn get_receipt(env: Env, payer: Address, index: u32) -> ReceiptMetadata {
        env.storage()
            .instance()
            .get(&DataKey::ReceiptRecord(payer, index))
            .expect("Receipt not found")
    }

    // ─── Streaming payment channels ────────────────────────────────────────

    /// Open a streaming payment channel from `payer` to `recipient`.
    ///
    /// `payer` must authorize (both to prove intent and to fund the deposit
    /// through the token contract) and `deposit` must be strictly positive.
    /// Returns the new stream id.
    pub fn open_stream(
        env: Env,
        payer: Address,
        recipient: Address,
        token: Address,
        rate_per_ledger: i128,
        deposit: i128,
    ) -> u32 {
        payer.require_auth();

        if rate_per_ledger <= 0 {
            panic!("Rate per ledger must be positive");
        }
        if deposit <= 0 {
            panic!("Deposit must be positive");
        }

        // Escrow the deposit into the contract
        let contract_address = env.current_contract_address();
        token::Client::new(&env, &token).transfer(&payer, &contract_address, &deposit);

        let stream_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::StreamCount)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::StreamCount, &stream_id.checked_add(1).expect("Stream id overflow"));

        let stream = Stream {
            payer: payer.clone(),
            recipient,
            token,
            rate_per_ledger,
            deposited: deposit,
            claimed: 0,
            start_ledger: env.ledger().sequence(),
        };
        env.storage().instance().set(&DataKey::Stream(stream_id), &stream);

        env.events().publish(
            (Symbol::new(&env, "stream_opened"), payer, stream_id),
            deposit,
        );

        stream_id
    }

    /// Claim all unclaimed streamed funds as the stream recipient.
    ///
    /// Only the stream's recipient can call this. Returns the amount claimed.
    pub fn claim_stream(env: Env, stream_id: u32, recipient: Address) -> i128 {
        recipient.require_auth();

        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .expect("Stream not found");
        if recipient != stream.recipient {
            panic!("Only the recipient can claim");
        }

        let (claimable, _) = stream_progress(&stream, env.ledger().sequence());
        if claimable > 0 {
            stream.claimed += claimable;
            env.storage().instance().set(&DataKey::Stream(stream_id), &stream);
            let contract_address = env.current_contract_address();
            token::Client::new(&env, &stream.token)
                .transfer(&contract_address, &recipient, &claimable);
        }

        claimable
    }

    /// Add funds to an existing stream.
    ///
    /// Only the stream's current payer can top up (their authorization also
    /// covers the token transfer). `amount` must be strictly positive.
    pub fn top_up_stream(env: Env, stream_id: u32, payer: Address, amount: i128) {
        payer.require_auth();

        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .expect("Stream not found");
        if payer != stream.payer {
            panic!("Only the payer can top up");
        }
        if amount <= 0 {
            panic!("Amount must be positive");
        }

        let contract_address = env.current_contract_address();
        token::Client::new(&env, &stream.token)
            .transfer(&payer, &contract_address, &amount);
        stream.deposited += amount;
        env.storage().instance().set(&DataKey::Stream(stream_id), &stream);

        env.events().publish(
            (Symbol::new(&env, "stream_topped_up"), payer, stream_id),
            amount,
        );
    }

    /// Close a stream and refund the unstreamed deposit to the payer.
    ///
    /// Only the stream's current payer can close. Returns the refund amount.
    pub fn close_stream(env: Env, stream_id: u32, payer: Address) -> i128 {
        payer.require_auth();

        let stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .expect("Stream not found");
        if payer != stream.payer {
            panic!("Only the payer can close");
        }

        let (_, refundable) = stream_progress(&stream, env.ledger().sequence());
        if refundable > 0 {
            let contract_address = env.current_contract_address();
            token::Client::new(&env, &stream.token)
                .transfer(&contract_address, &payer, &refundable);
        }

        // The stream is settled; the recipient keeps whatever was claimable
        // at this point, so the record can be removed.
        env.storage().instance().remove(&DataKey::Stream(stream_id));

        env.events().publish(
            (Symbol::new(&env, "stream_closed"), payer, stream_id),
            refundable,
        );

        refundable
    }

    /// Transfer top-up and close rights (the payer role) to `new_payer`.
    ///
    /// Both the current payer and the new payer must authorize, so the
    /// transfer can only happen with consent of both parties (e.g. a
    /// subscription handed over to a new subscriber). The recipient,
    /// escrowed funds and streaming rate are unchanged.
    pub fn transfer_stream(env: Env, stream_id: u32, payer: Address, new_payer: Address) {
        // Reject no-op transfers up front: the two require_auth calls below
        // would otherwise collide on the same authorization frame.
        if payer == new_payer {
            panic!("New payer must be different from the current payer");
        }

        // Both parties must sign the transfer
        payer.require_auth();
        new_payer.require_auth();

        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .expect("Stream not found");
        if payer != stream.payer {
            panic!("Only the payer can transfer");
        }

        stream.payer = new_payer.clone();
        env.storage().instance().set(&DataKey::Stream(stream_id), &stream);

        env.events().publish(
            (Symbol::new(&env, "stream_transferred"), payer, new_payer),
            stream_id,
        );
    }

    /// Get a stream by id, including who the current payer is.
    pub fn get_stream(env: Env, stream_id: u32) -> Stream {
        env.storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .expect("Stream not found")
    }

    /// Get the unclaimed amount currently claimable by the recipient.
    pub fn get_claimable(env: Env, stream_id: u32) -> i128 {
        let stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .expect("Stream not found");
        let (claimable, _) = stream_progress(&stream, env.ledger().sequence());
        claimable
    }

    // ─── Placeholders (future features) ──────────────────────────────────────

    /// [PLACEHOLDER] Create an escrow payment that releases after a time lock.
    /// See ROADMAP.md v2.1 — Soroban Escrow Payments.
    ///
    /// Future implementation:
    ///   - Lock funds in the contract
    ///   - Release to recipient after `release_ledger`
    ///   - Allow sender to cancel before release
    pub fn create_escrow(
        _env: Env,
        _from: Address,
        _to: Address,
        _amount: i128,
        _release_ledger: u32,
    ) {
        panic!("Escrow payments coming in v2.1 — see ROADMAP.md");
    }

    /// [PLACEHOLDER] Batch multiple micro-payments in a single transaction.
    /// See ROADMAP.md v2.0 — Multi-Currency Payments.
    pub fn batch_send(
        _env: Env,
        _from: Address,
        _recipients: soroban_sdk::Vec<Address>,
        _amounts: soroban_sdk::Vec<i128>,
    ) {
        panic!("Batch payments coming in v2.0 — see ROADMAP.md");
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, AuthorizedFunction, AuthorizedInvocation},
        Address, Env,
    };

    // The official Soroban pattern for `std::vec![]` auth assertions in a
    // `#![no_std]` crate (see stellar/soroban-examples liquidity pool tests).
    extern crate std;

    #[test]
    fn test_initialize() {
        let env = Env::default();
        let contract_id = env.register_contract(None, MicroPayContract);
        let client = MicroPayContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    #[should_panic(expected = "Contract already initialized")]
    fn test_double_initialize_fails() {
        let env = Env::default();
        let contract_id = env.register_contract(None, MicroPayContract);
        let client = MicroPayContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        client.initialize(&admin); // should panic
    }

    #[test]
    fn test_mint_receipt() {
        let env = Env::default();
        let contract_id = env.register_contract(None, MicroPayContract);
        let client = MicroPayContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let payer = Address::generate(&env);
        let payee = Address::generate(&env);

        env.mock_all_auths();

        let memo = Symbol::new(&env, "Rent");
        let receipt_id = client.mint_receipt(&payer, &payee, &1000, &memo);
        assert_eq!(receipt_id, 0);

        assert_eq!(client.get_receipt_count(&payer), 1);

        let stored = client.get_receipt(&payer, &0);
        assert_eq!(stored.from, payer);
        assert_eq!(stored.to, payee);
        assert_eq!(stored.amount, 1000);
        assert_eq!(stored.memo, memo);
    }

    #[test]
    fn test_receipt_count_tracks_multiple_mints() {
        let env = Env::default();
        let contract_id = env.register_contract(None, MicroPayContract);
        let client = MicroPayContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let payer = Address::generate(&env);
        let payee1 = Address::generate(&env);
        let payee2 = Address::generate(&env);

        env.mock_all_auths();

        let id1 = client.mint_receipt(&payer, &payee1, &500, &Symbol::new(&env, "Coffee"));
        let id2 = client.mint_receipt(&payer, &payee2, &1500, &Symbol::new(&env, "Invoice"));

        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(client.get_receipt_count(&payer), 2);
    }

    #[test]
    fn test_tip_totals_start_at_zero() {
        let env = Env::default();
        let contract_id = env.register_contract(None, MicroPayContract);
        let client = MicroPayContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let recipient = Address::generate(&env);
        assert_eq!(client.get_tip_total(&recipient), 0);
        assert_eq!(client.get_tip_count(&recipient), 0);
    }

    // ─── Streaming tests ──────────────────────────────────────────────────

    use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
    use soroban_sdk::testutils::{Ledger as _, MockAuth, MockAuthInvoke};
    use soroban_sdk::IntoVal;

    const RATE: i128 = 1_000; // stroops streamed per ledger
    const DEPOSIT: i128 = 1_000_000; // stroops escrowed when the stream opens
    const STARTING_BALANCE: i128 = 1_000_000_000; // stroops minted to each payer

    /// Register the contract, initialize it, and create a SAC token for the
    /// streaming tests. The returned clients borrow from `env`.
    fn setup(
        env: &Env,
    ) -> (
        MicroPayContractClient<'_>,
        TokenClient<'_>,
        StellarAssetClient<'_>,
    ) {
        let contract_id = env.register_contract(None, MicroPayContract);
        let client = MicroPayContractClient::new(env, &contract_id);

        let admin = Address::generate(env);
        client.initialize(&admin);

        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token = TokenClient::new(env, &sac.address());
        let token_admin = StellarAssetClient::new(env, &sac.address());

        (client, token, token_admin)
    }

    #[test]
    fn test_transfer_stream_updates_payer() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        // The transfer must be signed by BOTH the current and the new payer.
        client.transfer_stream(&stream_id, &payer, &new_payer);

        // Both parties must appear in the authorization tree, in call order.
        assert_eq!(
            env.auths(),
            std::vec![
                (
                    payer.clone(),
                    AuthorizedInvocation {
                        function: AuthorizedFunction::Contract((
                            client.address.clone(),
                            Symbol::new(&env, "transfer_stream"),
                            (stream_id, payer.clone(), new_payer.clone()).into_val(&env)
                        )),
                        sub_invocations: std::vec![]
                    }
                ),
                (
                    new_payer.clone(),
                    AuthorizedInvocation {
                        function: AuthorizedFunction::Contract((
                            client.address.clone(),
                            Symbol::new(&env, "transfer_stream"),
                            (stream_id, payer.clone(), new_payer.clone()).into_val(&env)
                        )),
                        sub_invocations: std::vec![]
                    }
                )
            ]
        );

        let stream = client.get_stream(&stream_id);
        assert_eq!(stream.payer, new_payer);
        assert_eq!(stream.recipient, recipient);
        assert_eq!(stream.deposited, DEPOSIT);
    }

    #[test]
    #[should_panic(expected = "Error(Auth, InvalidAction)")]
    fn test_transfer_stream_requires_both_signatures() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        // No auth entries at all — neither party signed, so the first
        // require_auth must reject the call.
        env.set_auths(&[]);
        client
            .transfer_stream(&stream_id, &payer, &new_payer);
    }

    #[test]
    #[should_panic(expected = "Error(Auth, InvalidAction)")]
    fn test_transfer_stream_requires_new_payer_signature() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        // Only the current payer signed — the new payer's signature is
        // missing, so the second require_auth must reject the call.
        env.mock_auths(&[MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "transfer_stream",
                args: (
                    stream_id,
                    payer.clone(),
                    new_payer.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client
            .transfer_stream(&stream_id, &payer, &new_payer);
    }

    #[test]
    fn test_former_payer_cannot_close_after_transfer() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        env.mock_all_auths();
        client
            .transfer_stream(&stream_id, &payer, &new_payer);

        // Advance one ledger so there is unstreamed deposit to refund.
        env.ledger().with_mut(|l| l.sequence_number += 1);

        // The original payer must no longer be able to close.
        assert!(client.try_close_stream(&stream_id, &payer).is_err());

        // The new payer can close and receives the refund.
        let refund = client.close_stream(&stream_id, &new_payer);
        assert_eq!(refund, DEPOSIT - RATE);
        assert_eq!(token.balance(&new_payer), STARTING_BALANCE + refund);
    }

    #[test]
    fn test_former_payer_cannot_top_up_after_transfer() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        env.mock_all_auths();
        client
            .transfer_stream(&stream_id, &payer, &new_payer);

        // The original payer must no longer be able to top up.
        assert!(client
            .try_top_up_stream(&stream_id, &payer, &100_000)
            .is_err());

        // The new payer can top up and the deposit grows.
        client
            .top_up_stream(&stream_id, &new_payer, &100_000);
        assert_eq!(client.get_stream(&stream_id).deposited, 1_100_000);
    }

    #[test]
    #[should_panic(expected = "New payer must be different")]
    fn test_transfer_stream_to_self_fails() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        client
            .transfer_stream(&stream_id, &payer, &payer);
    }

    #[test]
    #[should_panic(expected = "Only the payer can transfer")]
    fn test_non_payer_cannot_transfer_stream() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let attacker = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        env.mock_all_auths();
        client
            .transfer_stream(&stream_id, &attacker, &new_payer);
    }

    #[test]
    #[should_panic(expected = "Stream not found")]
    fn test_transfer_missing_stream_fails() {
        let env = Env::default();
        let (client, _token, _token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();

        client.transfer_stream(&999, &payer, &new_payer);
    }

    #[test]
    fn test_open_claim_and_close_stream() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);

        let payer_balance_before = token.balance(&payer);
        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );
        assert_eq!(stream_id, 0);
        assert_eq!(
            token.balance(&payer),
            payer_balance_before - DEPOSIT
        );

        // Advance 5 ledgers: 5 * RATE streamed so far.
        env.ledger().with_mut(|l| l.sequence_number += 5);
        assert_eq!(client.get_claimable(&stream_id), 5 * RATE);

        let claimed = client.claim_stream(&stream_id, &recipient);
        assert_eq!(claimed, 5 * RATE);
        assert_eq!(token.balance(&recipient), 5 * RATE);

        // Closing refunds the unstreamed remainder.
        let refund = client.close_stream(&stream_id, &payer);
        assert_eq!(refund, DEPOSIT - 5 * RATE);
        assert!(client.try_get_stream(&stream_id).is_err());
    }

    #[test]
    fn test_stream_flow_with_transfer() {
        let env = Env::default();
        let (client, token, token_admin) = setup(&env);

        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let new_payer = Address::generate(&env);

        env.mock_all_auths();
        token_admin.mint(&payer, &STARTING_BALANCE);
        token_admin.mint(&new_payer, &STARTING_BALANCE);

        let stream_id = client.open_stream(
            &payer,
            &recipient,
            &token.address,
            &RATE,
            &DEPOSIT,
        );

        // Advance 3 ledgers, then transfer to the new payer.
        env.ledger().with_mut(|l| l.sequence_number += 3);
        env.mock_all_auths();
        client
            .transfer_stream(&stream_id, &payer, &new_payer);

        // Streaming is unaffected by the transfer: 3 more ledgers stream.
        env.ledger().with_mut(|l| l.sequence_number += 3);
        assert_eq!(client.get_claimable(&stream_id), 6_000);

        // The new payer tops up the stream.
        client
            .top_up_stream(&stream_id, &new_payer, &500_000);
        assert_eq!(client.get_stream(&stream_id).deposited, DEPOSIT + 500_000);

        // The new payer closes and refunds everything not yet claimed.
        let refund = client.close_stream(&stream_id, &new_payer);
        assert_eq!(refund, DEPOSIT + 500_000 - 6 * RATE);
    }
}
