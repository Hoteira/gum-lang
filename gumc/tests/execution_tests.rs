use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use revm::context::result::{ExecutionResult, Output};
use revm::context::TxEnv;
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::{hardfork::SpecId, Address, TxKind, U256};
use revm::{Context, Database, ExecuteCommitEvm, MainBuilder, MainContext};

type Db = CacheDB<EmptyDB>;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

// Finds solc: $SOLC first, then tools/solc(.exe), then whatever is on PATH.
// The bare name is the fallback rather than the default so a local tools/ copy still wins, which is what keeps a developer's runs on the version this repo was verified against.
//
fn solc_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SOLC") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    for name in ["solc.exe", "solc"] {
        let p = repo_root().join("tools").join(name);
        if p.exists() {
            return Some(p);
        }
    }
    if Command::new("solc").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
        return Some(PathBuf::from("solc"));
    }
    assert!(
        std::env::var("GUM_REQUIRE_SOLC").is_err(),
        "GUM_REQUIRE_SOLC is set but no solc was found: checked $SOLC, tools/solc(.exe), and PATH"
    );
    None
}

fn tmp_path(ext: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("gum_exec_{}_{}.{}", std::process::id(), id, ext));
    p
}

// Compiles gum source all the way to creation bytecode via gumc --bytecode.
// With rich, enables Panic(uint256) revert data so revert bytes can be
// diffed against Solidity's.
fn gum_creation_bytecode(src: &str, solc: &Path, rich: bool) -> Vec<u8> {
    gum_creation_bytecode_for(src, solc, rich, "")
}

// Runs gumc without assembling, returning (succeeded, combined output), for
// asserting on diagnostics rather than bytecode.
fn run_gumc_exec(src: &str) -> (bool, String) {
    let path = tmp_path("gum");
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gumc"))
        .arg(&path)
        .output()
        .expect("failed to run gumc");
    let _ = std::fs::remove_file(&path);
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

// Creation bytecode for one contract out of a source that may declare several.
// name empty means "the first one" (the single-contract case).
fn gum_creation_bytecode_for(src: &str, solc: &Path, rich: bool, name: &str) -> Vec<u8> {
    let path = tmp_path("gum");
    std::fs::write(&path, src).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gumc"));
    cmd.arg(&path)
        .arg("--bytecode")
        .arg("--solc")
        .arg(solc);
    if rich {
        cmd.arg("--rich-reverts");
    }
    let out = cmd.output().expect("failed to run gumc");
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
    let is_hex = |l: &str| l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit());

    // gumc prints "--> [Assembler] <Name> EVM bytecode (N bytes):" then the hex.
    // Take the hex following the requested contract's banner; with no name, the
    // first hex line in the output.
    let mut lines = text.lines().map(str::trim);
    if !name.is_empty() {
        let banner = format!("[Assembler] {} EVM bytecode", name);
        lines
            .find(|l| l.contains(&banner))
            .unwrap_or_else(|| panic!("gumc emitted no bytecode for '{}':\n{}{}", name, text, String::from_utf8_lossy(&out.stderr)));
    }
    let hex = lines
        .find(|l| is_hex(l))
        .unwrap_or_else(|| panic!("no bytecode from gumc:\n{}{}", text, String::from_utf8_lossy(&out.stderr)));
    hex::decode(&hex[2..]).expect("bad gum hex")
}

// Like gum_creation_bytecode but under a storage lock at lock_path (created
// on first use, enforced after), for upgrade-safety tests.
fn gum_creation_bytecode_locked(src: &str, solc: &Path, lock_path: &Path) -> Vec<u8> {
    let path = tmp_path("gum");
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gumc"))
        .arg(&path)
        .arg("--bytecode")
        .arg("--solc")
        .arg(solc)
        .arg("--lock")
        .arg(lock_path)
        .output()
        .expect("failed to run gumc");
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
    let hex = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no bytecode from locked gumc:\n{}{}", text, String::from_utf8_lossy(&out.stderr)));
    hex::decode(&hex[2..]).expect("bad gum hex")
}

// Compiles a Solidity contract to creation bytecode via solc --bin.
fn sol_creation_bytecode(src: &str, solc: &Path) -> Vec<u8> {
    sol_creation_bytecode_for(src, solc, "")
}

// Same, selecting one contract out of a source that declares several. solc
// prints a "======= <path>:<Name> =======" banner before each.
fn sol_creation_bytecode_for(src: &str, solc: &Path, name: &str) -> Vec<u8> {
    let path = tmp_path("sol");
    std::fs::write(&path, src).unwrap();
    let out = Command::new(solc)
        .arg("--optimize")
        .arg("--no-cbor-metadata")
        .arg("--bin")
        .arg(&path)
        .output()
        .expect("failed to run solc");
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().map(str::trim);
    if !name.is_empty() {
        let banner = format!(":{} =======", name);
        lines
            .find(|l| l.contains(&banner))
            .unwrap_or_else(|| panic!("solc emitted no bytecode for '{}':\n{}{}", name, text, String::from_utf8_lossy(&out.stderr)));
    }
    let hex = lines
        .find(|l| l.len() > 40 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no bytecode from solc:\n{}{}", text, String::from_utf8_lossy(&out.stderr)));
    hex::decode(hex).expect("bad sol hex")
}

fn deployer() -> Address {
    Address::from([0x11u8; 20])
}

// Builds an EVM over $db pinned to the Osaka hardfork.
//
// The spec is load-bearing, not incidental: it's what makes EIP-7702
// delegation indicators and the EIP-7951 secp256r1 precompile at 0x100 exist
// at all. On an older fork those features silently don't, and the tests that
// cover them would be verifying nothing.
//
// A macro rather than a function because the concrete MainnetEvm<...> type
// is unnameable in practice and impl Trait loses the associated types the
// call sites need.
// Per-transaction gas limit. Osaka caps this at 2^24 (EIP-7825), so the old
// "just pass u64::MAX" trick is now a hard transaction error. Sits at the cap:
// far above anything these contracts need (the priciest deploy is ~550k).
const TX_GAS_LIMIT: u64 = 16_777_216;

macro_rules! evm_for {
    ($db:expr) => {
        Context::mainnet()
            .with_db(&mut *$db)
            .modify_cfg_chained(|c| {
                c.spec = SpecId::OSAKA;
                // These tests drive many transactions from one address and care
                // about contract behavior, not transaction plumbing. Both sides
                // of every differential run under this same config, so relaxing
                // the sender checks can't mask a gum-vs-Solidity difference.
                c.disable_nonce_check = true;
            })
            .build_mainnet()
    };
}

// Deploys creation bytecode; returns the new contract address.
fn deploy(db: &mut Db, creation: Vec<u8>) -> Address {
    deploy_with_gas(db, creation).0
}

// Deploys and also returns the gas the deployment transaction consumed.
fn deploy_with_gas(db: &mut Db, creation: Vec<u8>) -> (Address, u64) {
    let mut evm = evm_for!(db);
    let tx = TxEnv::builder()
        .caller(deployer())
        .kind(TxKind::Create)
        .data(creation.into())
        .value(U256::ZERO)
        .gas_limit(TX_GAS_LIMIT)
        .build()
        .expect("bad deploy tx");
    match evm.transact_commit(tx).expect("deploy tx failed") {
        ExecutionResult::Success { output: Output::Create(_, Some(addr)), gas, .. } => (addr, gas.tx_gas_used()),
        other => panic!("deployment did not create a contract: {:?}", other),
    }
}

struct CallResult {
    success: bool,
    output: Vec<u8>,
    // Each emitted log as (topics, data), so event encoding can be diffed.
    logs: Vec<(Vec<[u8; 32]>, Vec<u8>)>,
    // Gas the EVM charged for this call, the live consumption figure.
    gas: u64,
}

// Sends calldata to to from deployer() and commits the result.
fn call(db: &mut Db, to: Address, data: Vec<u8>) -> CallResult {
    call_from(db, deployer(), to, data)
}

// Sends calldata to to from an arbitrary caller and commits the result.
fn call_from(db: &mut Db, caller: Address, to: Address, data: Vec<u8>) -> CallResult {
    call_with_value(db, caller, to, data, U256::ZERO)
}

// Like call_from but attaches ETH, for exercising payable / nonpayable guards.
// The caller is funded first so the transfer itself can't be what fails.
fn call_with_value(db: &mut Db, caller: Address, to: Address, data: Vec<u8>, value: U256) -> CallResult {
    if value > U256::ZERO {
        let mut info = db.basic(caller).unwrap().unwrap_or_default();
        info.balance = info.balance.saturating_add(value).saturating_add(U256::from(10u64).pow(U256::from(18)));
        db.insert_account_info(caller, info);
    }
    let mut evm = evm_for!(db);
    let tx = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(to))
        .data(data.into())
        .value(value)
        .gas_limit(TX_GAS_LIMIT)
        .build()
        .expect("bad call tx");
    match evm.transact_commit(tx).expect("call tx failed") {
        ExecutionResult::Success { output, logs, gas, .. } => {
            let logs = logs.into_iter().map(|l| {
                let topics = l.data.topics().iter().map(|t| t.0).collect();
                (topics, l.data.data.to_vec())
            }).collect();
            CallResult { success: true, output: output.into_data().to_vec(), logs, gas: gas.tx_gas_used() }
        }
        ExecutionResult::Revert { output, gas, .. } => CallResult { success: false, output: output.to_vec(), logs: vec![], gas: gas.tx_gas_used() },
        ExecutionResult::Halt { gas, .. } => CallResult { success: false, output: vec![], logs: vec![], gas: gas.tx_gas_used() },
    }
}

fn selector(sig: &str) -> [u8; 4] {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(sig.as_bytes());
    k.finalize(&mut out);
    [out[0], out[1], out[2], out[3]]
}

fn encode(sig: &str, args: &[U256]) -> Vec<u8> {
    let mut v = selector(sig).to_vec();
    for a in args {
        v.extend_from_slice(&a.to_be_bytes::<32>());
    }
    v
}

fn word_u256(v: U256) -> [u8; 32] {
    v.to_be_bytes()
}

fn word_addr(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

// Selector + raw 32-byte words, for calls mixing address and uint256 args.
fn encode_words(sig: &str, words: &[[u8; 32]]) -> Vec<u8> {
    let mut v = selector(sig).to_vec();
    for w in words {
        v.extend_from_slice(w);
    }
    v
}

fn read_repo_file(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel)).unwrap()
}

fn storage(db: &mut Db, addr: Address, slot: u64) -> U256 {
    db.storage(addr, U256::from(slot)).expect("storage read failed")
}

fn storage_at(db: &mut Db, addr: Address, slot: U256) -> U256 {
    db.storage(addr, slot).expect("storage read failed")
}

// Storage slot of mapping[key] for a base slot < 256, per the standard EVM
// layout keccak256(pad32(key) . pad32(base)), the exact scheme both solc
// and gum's gum_hash_slot use, so a match here proves they agree.
fn mapping_slot(key: Address, base: u8) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_slice()); // key, left-padded
    buf[63] = base; // base slot, big-endian in the second word
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(&buf);
    k.finalize(&mut out);
    U256::from_be_bytes(out)
}

// Deterministic splitmix64 PRNG so fuzz failures reproduce exactly.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    // A random U256. wide occasionally yields near-max values to provoke
    // overflow paths; otherwise stays in u64 range for mostly-succeeding ops.
    fn next_u256(&mut self, wide: bool) -> U256 {
        if wide && self.next_u64() % 4 == 0 {
            // Near the top of the range: forces checked-arithmetic reverts.
            U256::MAX - U256::from(self.next_u64())
        } else {
            U256::from(self.next_u64())
        }
    }
}

// A minimal self-contained contract pair (no external calls) that exercises
// storage read/write, checked arithmetic, and ABI return encoding, the core
// of gum's codegen, in a form solc can mirror exactly.
const GUM_ARR_COPY: &str = include_str!("fixtures/gum_arr_copy.gum");
const GUM_STRUCT_ABI: &str = include_str!("fixtures/gum_struct_abi.gum");
const GUM_STRUCT_CTOR: &str = include_str!("fixtures/gum_struct_ctor.gum");
const GUM_STRUCT_DEPLOY: &str = include_str!("fixtures/gum_struct_deploy.gum");
const GUM_IFACE_CALL: &str = include_str!("fixtures/gum_iface_call.gum");
const GUM_MSG_BLOCK: &str = include_str!("fixtures/gum_msg_block.gum");
const GUM_ENUM_ABI: &str = include_str!("fixtures/gum_enum_abi.gum");
const GUM_ENUM_STATE: &str = include_str!("fixtures/gum_enum_state.gum");
const SOL_ENUM_STATE: &str = include_str!("fixtures/sol_enum_state.sol");
const GUM_VIEW: &str = include_str!("fixtures/gum_view.gum");
const SOL_PROBER: &str = include_str!("fixtures/sol_prober.sol");
const SOL_ENUM_ABI: &str = include_str!("fixtures/sol_enum_abi.sol");
const SOL_MSG_BLOCK: &str = include_str!("fixtures/sol_msg_block.sol");
const GUM_STARR_ABI: &str = include_str!("fixtures/gum_starr_abi.gum");
const GUM_NEST_ABI: &str = include_str!("fixtures/gum_nest_abi.gum");
const GUM_LOG_NONSCALAR: &str = include_str!("fixtures/gum_log_nonscalar.gum");
const SOL_LOG_NONSCALAR: &str = include_str!("fixtures/sol_log_nonscalar.sol");
const SOL_NEST_ABI: &str = include_str!("fixtures/sol_nest_abi.sol");
const SOL_STARR_ABI: &str = include_str!("fixtures/sol_starr_abi.sol");
const SOL_IFACE_SINK: &str = include_str!("fixtures/sol_iface_sink.sol");
const SOL_STRUCT_DEPLOY: &str = include_str!("fixtures/sol_struct_deploy.sol");
const SOL_STRUCT_CTOR: &str = include_str!("fixtures/sol_struct_ctor.sol");
const SOL_STRUCT_ABI: &str = include_str!("fixtures/sol_struct_abi.sol");
const GUM_STORE: &str = include_str!("fixtures/gum_store.gum");

const SOL_STORE: &str = include_str!("fixtures/sol_store.sol");

const GUM_STRING_ECHO: &str = include_str!("fixtures/gum_string_echo.gum");

const SOL_STRING_ECHO: &str = include_str!("fixtures/sol_string_echo.sol");

const GUM_STORE_CONSTRUCTOR: &str = include_str!("fixtures/gum_store_constructor.gum");

// Mixed static + multiple dynamic args, plus a dynamic return. Exercises the
// ABI head/tail split: which sits inline in the head, a and b are offset
// pointers into the tail; the chosen one is re-encoded as a dynamic return.
const GUM_ABI_MIX: &str = include_str!("fixtures/gum_abi_mix.gum");

const SOL_ABI_MIX: &str = include_str!("fixtures/sol_abi_mix.sol");

const SOL_STORE_CONSTRUCTOR: &str = include_str!("fixtures/sol_store_constructor.sol");

// Deploys both, replays the same call vector on each, and asserts identical
// success, return data, and slot-0 storage after every step. rich compiles
// gum with --rich-reverts so revert reason bytes are comparable too.
fn diff_run(calls: &[(&str, Vec<U256>)], rich: bool) {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping execution diff: no solc found");
            return;
        }
    };

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STORE, &solc, rich));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STORE, &solc));

    for (sig, args) in calls {
        let data = encode(sig, args);
        let gr = call(&mut gdb, gaddr, data.clone());
        let sr = call(&mut sdb, saddr, data);

        assert_eq!(gr.success, sr.success, "success mismatch on {}: gum={} sol={}", sig, gr.success, sr.success);
        assert_eq!(gr.output, sr.output, "return-data mismatch on {}:\n gum={:02x?}\n sol={:02x?}", sig, gr.output, sr.output);

        let gs = storage(&mut gdb, gaddr, 0);
        let ss = storage(&mut sdb, saddr, 0);
        assert_eq!(gs, ss, "slot-0 storage mismatch after {}: gum={} sol={}", sig, gs, ss);
    }
}

#[test]
fn store_set_add_get_matches_solidity() {
    diff_run(&[
        ("set(uint256)", vec![U256::from(5)]),
        ("add(uint256)", vec![U256::from(37)]),
        ("get()", vec![]),
    ], false);
}

#[test]
fn store_get_returns_correct_value() {
    // Isolate ABI return encoding: after set(99), get() must return exactly
    // the 32-byte big-endian 99 on both compilers.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STORE, &solc, false));
    call(&mut gdb, gaddr, encode("set(uint256)", &[U256::from(99)]));
    let r = call(&mut gdb, gaddr, encode("get()", &[]));
    assert!(r.success, "get() reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(99), "gum get() returned wrong value");
}

#[test]
fn constructor_initializes_storage() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    
    let mut g_creation = gum_creation_bytecode(GUM_STORE_CONSTRUCTOR, &solc, false);
    g_creation.extend_from_slice(&U256::from(999).to_be_bytes::<32>());
    let gaddr = deploy(&mut gdb, g_creation);
    
    let mut s_creation = sol_creation_bytecode(SOL_STORE_CONSTRUCTOR, &solc);
    s_creation.extend_from_slice(&U256::from(999).to_be_bytes::<32>());
    let saddr = deploy(&mut sdb, s_creation);

    let gr = call(&mut gdb, gaddr, encode("get()", &[]));
    let sr = call(&mut sdb, saddr, encode("get()", &[]));
    assert!(gr.success, "gum get() reverted");
    assert!(sr.success, "sol get() reverted");
    assert_eq!(gr.output, sr.output, "gum and sol get() return data mismatch");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(999), "constructor did not initialize storage correctly");
}

#[test]
fn string_abi_decoding_and_encoding_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STRING_ECHO, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STRING_ECHO, &solc));

    // Encode a dynamic string: "Hello, Gum World! This is a dynamic string!"
    // length = 43
    // Offset = 32
    let message = b"Hello, Gum World! This is a dynamic string!";
    let mut data = selector("echo(string)").to_vec();
    data.extend_from_slice(&U256::from(32).to_be_bytes::<32>()); // head offset
    data.extend_from_slice(&U256::from(message.len()).to_be_bytes::<32>()); // tail length
    data.extend_from_slice(message);
    // Pad to 32 bytes
    let pad_len = (32 - (message.len() % 32)) % 32;
    data.extend(vec![0u8; pad_len]);

    let gr = call(&mut gdb, gaddr, data.clone());
    let sr = call(&mut sdb, saddr, data.clone());

    assert!(gr.success, "gum echo() reverted");
    assert!(sr.success, "sol echo() reverted");
    assert_eq!(gr.output, sr.output, "gum and sol echo() return data mismatch");

    // Also test get_len to ensure the length is properly available in the String object!
    let mut data2 = selector("get_len(string)").to_vec();
    data2.extend_from_slice(&data[4..]); // reuse the encoded string argument
    
    let gr2 = call(&mut gdb, gaddr, data2.clone());
    let sr2 = call(&mut sdb, saddr, data2);
    
    assert!(gr2.success, "gum get_len() reverted");
    assert!(sr2.success, "sol get_len() reverted");
    assert_eq!(gr2.output, sr2.output, "gum and sol get_len() return data mismatch");
}

#[test]
fn short_calldata_reverts_like_solidity() {
    // Regression for the ABI-decode gap: calling set(uint256) with only the
    // 4-byte selector and no argument data. Solidity reverts (calldata too
    // short); gum must now do the same rather than decode a zero-padded
    // garbage argument and succeed. This test would have failed before the
    // per-function calldatasize guard was added, the harness drove the fix.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STORE, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STORE, &solc));

    let bare_selector = selector("set(uint256)").to_vec(); // no argument word
    let gr = call(&mut gdb, gaddr, bare_selector.clone());
    let sr = call(&mut sdb, saddr, bare_selector);

    assert!(!sr.success, "sanity: Solidity should revert on short calldata");
    assert_eq!(gr.success, sr.success, "gum must also revert on short calldata (got success={})", gr.success);
}

// A universal mock token: any call (transferFrom, transfer, whatever selector)
// returns a 32-byte true. Enough to satisfy both amms' external-call paths
// and their returndata/bool checks, without reimplementing an ERC20.
const SOL_MOCK: &str = include_str!("fixtures/sol_mock.sol");

fn assert_logs_match(sig: &str, g: &CallResult, s: &CallResult) {
    assert_eq!(g.logs.len(), s.logs.len(), "log count mismatch after {}: gum={} sol={}", sig, g.logs.len(), s.logs.len());
    for (i, (gl, sl)) in g.logs.iter().zip(s.logs.iter()).enumerate() {
        assert_eq!(gl.0, sl.0, "log {} topics differ after {}", i, sig);
        assert_eq!(gl.1, sl.1, "log {} data differ after {}", i, sig);
    }
}

#[test]
fn amm_external_calls_storage_and_events_match_solidity() {
    // The real memory-stressing path: external ERC20 calls + indexed events.
    // Deploy two mock tokens and the amm (same order/nonces => matching
    // addresses across both EVMs), then drive identical initialize /
    // add_liquidity / swap on gum and solc output and diff success, return
    // data, scalar storage (reserves + total_shares), and emitted logs.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mock = sol_creation_bytecode(SOL_MOCK, &solc);
    let gum_amm = gum_creation_bytecode(&read_repo_file("examples/amm.gum"), &solc, true);
    let sol_amm = sol_creation_bytecode(&read_repo_file("examples/solidity/amm.sol"), &solc);

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());

    let ga = deploy(&mut gdb, mock.clone());
    let gb = deploy(&mut gdb, mock.clone());
    let gamm = deploy(&mut gdb, gum_amm);
    let sa = deploy(&mut sdb, mock.clone());
    let sb = deploy(&mut sdb, mock.clone());
    let samm = deploy(&mut sdb, sol_amm);
    // CREATE addresses are a function of (deployer, nonce); identical deploy
    // order => identical addresses, so one calldata drives both sides.
    assert_eq!((ga, gb, gamm), (sa, sb, samm), "deploy addresses diverged");

    let steps: Vec<(&str, Vec<[u8; 32]>)> = vec![
        ("initialize(address,address)", vec![word_addr(ga), word_addr(gb)]),
        ("add_liquidity(uint256,uint256)", vec![word_u256(U256::from(1000)), word_u256(U256::from(2000))]),
        ("swap(address,uint256)", vec![word_addr(ga), word_u256(U256::from(500))]),
    ];

    for (sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call(&mut gdb, gamm, data.clone());
        let sr = call(&mut sdb, samm, data);

        assert_eq!(gr.success, sr.success, "success mismatch on {}: gum={} sol={}", sig, gr.success, sr.success);
        assert!(gr.success, "{} reverted unexpectedly on gum", sig);
        assert_eq!(gr.output, sr.output, "return data mismatch on {}", sig);
        assert_logs_match(sig, &gr, &sr);

        // reserve_a=2, reserve_b=3, total_shares=4, deterministic scalar slots
        // in both layouts.
        for slot in [2u64, 3, 4] {
            let gs = storage(&mut gdb, gamm, slot);
            let ss = storage(&mut sdb, samm, slot);
            assert_eq!(gs, ss, "storage slot {} mismatch after {}: gum={} sol={}", slot, sig, gs, ss);
        }

        // shares[sender] lives in the mapping at base slot 5. Diffing it here
        // proves gum's gum_hash_slot hashes keys identically to solc's mapping
        // layout, if they diverged, gum's value would sit at a different slot
        // and read as 0 against Solidity's non-zero.
        let mslot = mapping_slot(deployer(), 5);
        let gm = storage_at(&mut gdb, gamm, mslot);
        let sm = storage_at(&mut sdb, samm, mslot);
        assert_eq!(gm, sm, "shares[sender] mapping slot mismatch after {}: gum={} sol={}", sig, gm, sm);
    }

    // Concrete check that the mapping actually holds the expected value, not
    // just that both sides agree on (possibly) zero.
    assert_eq!(storage_at(&mut gdb, gamm, mapping_slot(deployer(), 5)), U256::from(1000), "shares[sender]");

    // Final sanity on the actual numbers: add_liquidity(1000,2000) then
    // swap(tokenA,500): reserveA = 1000+500, reserveB = 2000 - (2000500/1500).
    assert_eq!(storage(&mut gdb, gamm, 2), U256::from(1500), "reserve_a");
    assert_eq!(storage(&mut gdb, gamm, 3), U256::from(2000u64 - (2000u64 * 500 / 1500)), "reserve_b");
}

#[test]
fn overflow_reverts_with_matching_panic_data() {
    // With --rich-reverts, gum's overflow revert must be byte-identical to
    // Solidity's Panic(0x11): same success=false AND same 36-byte reason.
    diff_run(&[
        ("set(uint256)", vec![U256::MAX]),
        ("add(uint256)", vec![U256::from(1)]),
    ], true);
}

// Micro-benchmark: read the same mapping key 5 times in one function. Probes
// whether solc's optimizer already CSEs gum's repeated gum_hash_slot + sload
// down to one keccak + one warm SLOAD (as it does for Solidity's m[k]+m[k]...).
// If gum's gas >> sol's, there's a real slot-caching win to implement.
const GUM_MAP_CSE: &str = include_str!("fixtures/gum_map_cse.gum");

const SOL_MAP_CSE: &str = include_str!("fixtures/sol_map_cse.sol");

// Probe gum's automatic storage packing vs Solidity's declaration-order
// layout. Fields are declared in a deliberately cache-unfriendly order
// (small, big, small): Solidity keeps that order, a(slot0), big(slot1),
// b(slot2) = 3 slots. gum reorders by size, big(slot0), a+b(slot1) = 2 slots
// , so writing all three touches one fewer cold slot (~one fewer 20k SSTORE).
const GUM_PACK: &str = include_str!("fixtures/gum_pack.gum");

const SOL_PACK: &str = include_str!("fixtures/sol_pack.sol");

#[test]
fn gas_probe_storage_packing() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(GUM_PACK, &solc, false);
    let sol = sol_creation_bytecode(SOL_PACK, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum);
    let saddr = deploy(&mut sdb, sol);
    let args = encode_words("setall(uint128,uint256,uint128)", &[
        word_u256(U256::from(7u64)), word_u256(U256::from(9u64)), word_u256(U256::from(11u64)),
    ]);
    let g = call(&mut gdb, gaddr, args.clone());
    let s = call(&mut sdb, saddr, args);
    println!("\n[probe] setall (3 fields):  gum {}  sol {}  (delta {:+})", g.gas, s.gas, g.gas as i64 - s.gas as i64);
    assert!(g.success && s.success, "gum={} sol={}", g.success, s.success);
}

#[test]
fn gas_probe_mapping_cse() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(GUM_MAP_CSE, &solc, false);
    let sol = sol_creation_bytecode(SOL_MAP_CSE, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum);
    let saddr = deploy(&mut sdb, sol);
    let k = word_addr(Address::from([0x77u8; 20]));
    let g = call(&mut gdb, gaddr, encode_words("sum5(address)", &[k]));
    let s = call(&mut sdb, saddr, encode_words("sum5(address)", &[k]));
    println!("\n[probe] mapping read x5:  gum {}  sol {}  (delta {:+})", g.gas, s.gas, g.gas as i64 - s.gas as i64);
    assert!(g.success && s.success);
}

// v1: two u128s pack into slot 0. v2: a u256 is prepended (worst case for
// reordering) and appended fields added. Under a lock, a/b must stay at
// slot 0 so v2's code reads the storage v1 wrote.
const LOCK_V1: &str = include_str!("fixtures/lock_v1.gum");

const LOCK_V2: &str = include_str!("fixtures/lock_v2.gum");

#[test]
fn storage_lock_keeps_v1_storage_readable_by_v2() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let lock = tmp_path("json");
    let _ = std::fs::remove_file(&lock);
    // v1 creates the lock; v2 is compiled under it (a/b pinned, big appended).
    let v1 = gum_creation_bytecode_locked(LOCK_V1, &solc, &lock);
    let v2 = gum_creation_bytecode_locked(LOCK_V2, &solc, &lock);
    let _ = std::fs::remove_file(&lock);

    // Deploy v1, write a=7,b=11, and capture the raw slot-0 word it produced.
    let mut db1: Db = CacheDB::new(EmptyDB::default());
    let a1 = deploy(&mut db1, v1);
    call(&mut db1, a1, encode("set_ab(uint128,uint128)", &[U256::from(7u64), U256::from(11u64)]));
    let v1_slot0 = storage(&mut db1, a1, 0);
    assert_eq!(U256::from_be_slice(&call(&mut db1, a1, encode("get_a()", &[])).output), U256::from(7u64));

    // Simulate an upgrade: deploy v2 fresh, then seed it with the exact
    // storage v1 left behind. If the lock held, v2's get_a/get_b read the
    // inherited slot-0 correctly and big (fresh slot) reads zero.
    let mut db2: Db = CacheDB::new(EmptyDB::default());
    let a2 = deploy(&mut db2, v2);
    db2.insert_account_storage(a2, U256::from(0u64), v1_slot0).unwrap();

    let ga = U256::from_be_slice(&call(&mut db2, a2, encode("get_a()", &[])).output);
    let gb = U256::from_be_slice(&call(&mut db2, a2, encode("get_b()", &[])).output);
    let gbig = U256::from_be_slice(&call(&mut db2, a2, encode("get_big()", &[])).output);
    assert_eq!(ga, U256::from(7u64), "v2 misread v1's a, layout drifted despite the lock");
    assert_eq!(gb, U256::from(11u64), "v2 misread v1's b, layout drifted despite the lock");
    assert_eq!(gbig, U256::ZERO, "appended field big should occupy a fresh, zero slot");
}

fn print_gas_row(label: &str, gum: u64, sol: u64) {
    let delta = gum as i64 - sol as i64;
    let pct = if sol > 0 { gum as f64 / sol as f64 * 100.0 } else { 0.0 };
    println!("  {:<30} gum {:>7}   sol {:>7}   delta {:>+6}   ({:.0}% of sol)", label, gum, sol, delta, pct);
}

// Deploys gum + sol, runs steps from the deployer, printing deploy + per-call
// gas. For contracts needing no external mocks or special callers.
fn gas_contract(solc: &Path, name: &str, gum_src: &str, sol_src: &str, steps: &[(&str, Vec<[u8; 32]>)]) {
    let gum = gum_creation_bytecode(gum_src, solc, false);
    let sol = sol_creation_bytecode(sol_src, solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let (ga, gdep) = deploy_with_gas(&mut gdb, gum);
    let (sa, sdep) = deploy_with_gas(&mut sdb, sol);
    println!("\n-- {} --", name);
    print_gas_row("deploy", gdep, sdep);
    for (sig, words) in steps {
        let g = call(&mut gdb, ga, encode_words(sig, words));
        let s = call(&mut sdb, sa, encode_words(sig, words));
        assert!(g.success && s.success, "{} {}: failed", name, sig);
        print_gas_row(sig, g.gas, s.gas);
    }
}

// Runtime bytecode size, gum vs Solidity, for every reference contract.
//
// cargo test --test execution_tests size_report -- --nocapture
//
// Unlike gas_report this asserts the range the README publishes. The size
// figures had drifted (the README claimed 62-93% and 63-94% in two places,
// while the truth was 62-95%) precisely because nothing checked them, whereas
// the gas numbers next door stayed accurate, they have a test. A published
// number with no test is a number that is already rotting.
#[test]
fn size_report() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping size report: no solc");
            return;
        }
    };
    // Solidity appends ~54 bytes of CBOR metadata by default; gum emits none.
    // Comparing against that would credit gum for bytes it simply doesn't
    // write, and the README's claim is about codegen density, so measure
    // solc without it. This is the comparison that can be lost, not won by
    // default.
    let cases: [(&str, &str); 5] = [
        ("token", "examples/solidity/token.sol"),
        ("amm", "examples/solidity/amm.sol"),
        ("erc20", "examples/solidity/erc20.sol"),
        ("erc721", "examples/solidity/erc721.sol"),
        ("vault", "examples/solidity/vault.sol"),
    ];
    let sources: [(&str, &str); 5] = [
        ("token", "examples/token.gum"),
        ("amm", "examples/amm.gum"),
        ("erc20", "examples/erc20.gum"),
        ("erc721", "examples/erc721.gum"),
        ("vault", "examples/vault.gum"),
    ];

    println!("\n===== SIZE: gum vs Solidity runtime bytecode (no metadata) =====");
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for ((name, sol_path), (_, gum_path)) in cases.iter().zip(sources.iter()) {
        let gum_src = read_repo_file(gum_path);
        let sol_src = read_repo_file(sol_path);
        // Deploy both and measure the code that actually lands on chain,
        // rather than parsing the creation preamble for a length.
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let gaddr = deploy(&mut gdb, gum_creation_bytecode(&gum_src, &solc, false));
        let saddr = deploy(&mut sdb, sol_creation_bytecode(&sol_src, &solc));
        let g = gdb.basic(gaddr).unwrap().unwrap().code.unwrap().len() as u64;
        let s = sdb.basic(saddr).unwrap().unwrap().code.unwrap().len() as u64;
        let pct = g * 100 / s;
        lo = lo.min(pct);
        hi = hi.max(pct);
        println!("  {:<8} gum {:>5}   sol {:>5}   {:>3}% of sol", name, g, s, pct);
    }
    println!("  range: {}-{}% of Solidity", lo, hi);

    // The band the README publishes. Widened only with a matching doc edit ,
    // failing here means the docs now lie, which is the whole point.
    assert!(lo >= 50 && hi <= 130, "size range {}-{}% is outside the documented 50-130% band", lo, hi);
}

// Live gas comparison, gum vs Solidity, executed in revm. Not an assertion
// test, run it to see the numbers:
// cargo test --test execution_tests gas_report -- --nocapture
// Gas figures include the 21000 intrinsic tx cost (identical on both sides,
// so deltas reflect real execution/deploy differences).
#[test]
fn gas_report() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping gas report: no solc");
            return;
        }
    };

    println!("\n===== GAS: gum vs Solidity (revm) =====");

    // ---- token ----
    {
        let gum = gum_creation_bytecode(&read_repo_file("examples/token.gum"), &solc, false);
        let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/token.sol"), &solc);
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let (gaddr, gdep) = deploy_with_gas(&mut gdb, gum);
        let (saddr, sdep) = deploy_with_gas(&mut sdb, sol);
        let to = Address::from([0x55u8; 20]);
        println!("\n-- token --");
        print_gas_row("deploy", gdep, sdep);
        let steps: Vec<(&str, Vec<[u8; 32]>)> = vec![
            ("initialize(uint256)", vec![word_u256(U256::from(1_000_000u64))]),
            ("transfer(address,uint256)", vec![word_addr(to), word_u256(U256::from(100u64))]),
            ("mint(address,uint256)", vec![word_addr(to), word_u256(U256::from(50u64))]),
        ];
        for (sig, words) in &steps {
            let g = call(&mut gdb, gaddr, encode_words(sig, words));
            let s = call(&mut sdb, saddr, encode_words(sig, words));
            assert!(g.success && s.success, "{} failed (gum={}, sol={})", sig, g.success, s.success);
            print_gas_row(sig, g.gas, s.gas);
        }
    }

    let sp = word_addr(Address::from([0x31u8; 20]));
    gas_contract(&solc, "erc20", &read_repo_file("examples/erc20.gum"), &read_repo_file("examples/solidity/erc20.sol"), &[
        ("init(uint256)", vec![word_u256(U256::from(1_000_000u64))]),
        ("approve(address,uint256)", vec![sp, word_u256(U256::from(500u64))]),
        ("transfer(address,uint256)", vec![sp, word_u256(U256::from(100u64))]),
    ]);
    gas_contract(&solc, "erc721", &read_repo_file("examples/erc721.gum"), &read_repo_file("examples/solidity/erc721.sol"), &[
        ("mint(address,uint256)", vec![word_addr(deployer()), word_u256(U256::from(1u64))]),
        ("approve(address,uint256)", vec![sp, word_u256(U256::from(1u64))]),
        ("setApprovalForAll(address,bool)", vec![sp, word_u256(U256::from(1u64))]),
    ]);
    gas_contract(&solc, "vault", &read_repo_file("examples/vault.gum"), &read_repo_file("examples/solidity/vault.sol"), &[
        ("deposit(uint256,uint256)", vec![word_u256(U256::from(100u64)), word_u256(U256::from(5000u64))]),
        ("withdraw(uint256)", vec![word_u256(U256::from(30u64))]),
    ]);
    gas_contract(&solc, "dyn_array", GUM_DYN_ARRAY, SOL_DYN_ARRAY, &[
        ("push_val(uint256)", vec![word_u256(U256::from(7u64))]),
        ("push_val(uint256)", vec![word_u256(U256::from(8u64))]),
        ("set_at(uint256,uint256)", vec![word_u256(U256::from(0u64)), word_u256(U256::from(9u64))]),
    ]);

    // ---- amm ----
    {
        let mock = sol_creation_bytecode(SOL_MOCK, &solc);
        let gum = gum_creation_bytecode(&read_repo_file("examples/amm.gum"), &solc, false);
        let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/amm.sol"), &solc);
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, mock.clone());
        let gb = deploy(&mut gdb, mock.clone());
        let (gamm, gdep) = deploy_with_gas(&mut gdb, gum);
        let sa = deploy(&mut sdb, mock.clone());
        let sb = deploy(&mut sdb, mock.clone());
        let (samm, sdep) = deploy_with_gas(&mut sdb, sol);
        assert_eq!((ga, gb, gamm), (sa, sb, samm));
        println!("\n-- amm --");
        print_gas_row("deploy", gdep, sdep);
        let steps: Vec<(&str, Vec<[u8; 32]>)> = vec![
            ("initialize(address,address)", vec![word_addr(ga), word_addr(gb)]),
            ("add_liquidity(uint256,uint256)", vec![word_u256(U256::from(1000u64)), word_u256(U256::from(2000u64))]),
            ("swap(address,uint256)", vec![word_addr(ga), word_u256(U256::from(500u64))]),
        ];
        for (sig, words) in &steps {
            let g = call(&mut gdb, gamm, encode_words(sig, words));
            let s = call(&mut sdb, samm, encode_words(sig, words));
            assert!(g.success && s.success, "{} failed (gum={}, sol={})", sig, g.success, s.success);
            print_gas_row(sig, g.gas, s.gas);
        }
    }
    println!();
}

fn time_it(reps: u32, mut f: impl FnMut()) -> std::time::Duration {
    let start = std::time::Instant::now();
    for _ in 0..reps {
        f();
    }
    start.elapsed()
}

fn print_time_row(label: &str, gum: std::time::Duration, sol: std::time::Duration, reps: u32) {
    let g = gum.as_nanos() as f64 / reps as f64 / 1000.0; // us per rep
    let s = sol.as_nanos() as f64 / reps as f64 / 1000.0;
    println!("  {:<26} gum {:>8.2}us   sol {:>8.2}us   ({:.0}% of sol)", label, g, s, g / s * 100.0);
}

// Times deploy + steps (fresh state each rep) for a no-setup contract.
fn time_contract(solc: &Path, name: &str, gum_src: &str, sol_src: &str, steps: &[(&str, Vec<[u8; 32]>)], reps: u32) {
    let gum = gum_creation_bytecode(gum_src, solc, false);
    let sol = sol_creation_bytecode(sol_src, solc);
    let run = |bc: &Vec<u8>| {
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let a = deploy(&mut db, bc.clone());
        for (sig, words) in steps {
            call(&mut db, a, encode_words(sig, words));
        }
    };
    let gt = time_it(reps, || run(&gum));
    let st = time_it(reps, || run(&sol));
    print_time_row(name, gt, st, reps);
}

// Wall-clock execution time in revm, gum vs Solidity, averaged over many
// deploy+call reps on fresh state each time. Run it with:
// cargo test --test execution_tests timing_report -- --nocapture
// CAVEAT: wall-clock is NOT the on-chain cost (gas is) and is noisy; treat
// this as a rough relative signal only. It broadly tracks opcode/gas counts.
#[test]
fn timing_report() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping timing report: no solc");
            return;
        }
    };
    const REPS: u32 = 1000;
    println!("\n===== EXECUTION TIME (revm wall-clock, avg of {} reps) =====", REPS);

    // ---- token: deploy + initialize + transfer + mint, fresh state each rep ----
    {
        let gum = gum_creation_bytecode(&read_repo_file("examples/token.gum"), &solc, false);
        let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/token.sol"), &solc);
        let to = Address::from([0x55u8; 20]);
        let init = encode_words("initialize(uint256)", &[word_u256(U256::from(1_000_000u64))]);
        let xfer = encode_words("transfer(address,uint256)", &[word_addr(to), word_u256(U256::from(100u64))]);
        let mint = encode_words("mint(address,uint256)", &[word_addr(to), word_u256(U256::from(50u64))]);

        let gt = time_it(REPS, || {
            let mut db: Db = CacheDB::new(EmptyDB::default());
            let a = deploy(&mut db, gum.clone());
            call(&mut db, a, init.clone());
            call(&mut db, a, xfer.clone());
            call(&mut db, a, mint.clone());
        });
        let st = time_it(REPS, || {
            let mut db: Db = CacheDB::new(EmptyDB::default());
            let a = deploy(&mut db, sol.clone());
            call(&mut db, a, init.clone());
            call(&mut db, a, xfer.clone());
            call(&mut db, a, mint.clone());
        });
        print_time_row("token (deploy+3 calls)", gt, st, REPS);
    }

    let sp = word_addr(Address::from([0x31u8; 20]));
    time_contract(&solc, "erc20 (deploy+3)", &read_repo_file("examples/erc20.gum"), &read_repo_file("examples/solidity/erc20.sol"), &[
        ("init(uint256)", vec![word_u256(U256::from(1_000_000u64))]),
        ("approve(address,uint256)", vec![sp, word_u256(U256::from(500u64))]),
        ("transfer(address,uint256)", vec![sp, word_u256(U256::from(100u64))]),
    ], REPS);
    time_contract(&solc, "erc721 (deploy+2)", &read_repo_file("examples/erc721.gum"), &read_repo_file("examples/solidity/erc721.sol"), &[
        ("mint(address,uint256)", vec![word_addr(deployer()), word_u256(U256::from(1u64))]),
        ("approve(address,uint256)", vec![sp, word_u256(U256::from(1u64))]),
    ], REPS);
    time_contract(&solc, "vault (deploy+2)", &read_repo_file("examples/vault.gum"), &read_repo_file("examples/solidity/vault.sol"), &[
        ("deposit(uint256,uint256)", vec![word_u256(U256::from(100u64)), word_u256(U256::from(5000u64))]),
        ("withdraw(uint256)", vec![word_u256(U256::from(30u64))]),
    ], REPS);

    // ---- amm: 2 mocks + amm deploy + initialize + add_liquidity + swap ----
    {
        let mock = sol_creation_bytecode(SOL_MOCK, &solc);
        let gum = gum_creation_bytecode(&read_repo_file("examples/amm.gum"), &solc, false);
        let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/amm.sol"), &solc);

        let run = |amm: &Vec<u8>| {
            let mut db: Db = CacheDB::new(EmptyDB::default());
            let a = deploy(&mut db, mock.clone());
            let b = deploy(&mut db, mock.clone());
            let amm_addr = deploy(&mut db, amm.clone());
            call(&mut db, amm_addr, encode_words("initialize(address,address)", &[word_addr(a), word_addr(b)]));
            call(&mut db, amm_addr, encode_words("add_liquidity(uint256,uint256)", &[word_u256(U256::from(1000u64)), word_u256(U256::from(2000u64))]));
            call(&mut db, amm_addr, encode_words("swap(address,uint256)", &[word_addr(a), word_u256(U256::from(500u64))]));
        };
        let gt = time_it(REPS, || run(&gum));
        let st = time_it(REPS, || run(&sol));
        print_time_row("amm (deploy+3 calls)", gt, st, REPS);
    }
    println!();
}

#[test]
fn fuzz_store_matches_solidity() {
    // Hundreds of random set/add ops against Store, diffed step-by-step. Wide
    // values regularly hit checked-add overflow, so this also asserts gum and
    // solc revert on the same inputs (and leave storage unchanged), with
    // --rich-reverts so the Panic reason bytes are compared too.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STORE, &solc, true));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STORE, &solc));

    let mut rng = Rng(0xdeadbeef);
    for i in 0..250 {
        let v = rng.next_u256(true);
        let sig = if rng.next_u64() % 2 == 0 { "set(uint256)" } else { "add(uint256)" };
        let data = encode(sig, &[v]);
        let gr = call(&mut gdb, gaddr, data.clone());
        let sr = call(&mut sdb, saddr, data);

        assert_eq!(gr.success, sr.success, "iter {}: success mismatch on {}({})", i, sig, v);
        assert_eq!(gr.output, sr.output, "iter {}: output mismatch on {}({})", i, sig, v);
        assert_eq!(
            storage(&mut gdb, gaddr, 0),
            storage(&mut sdb, saddr, 0),
            "iter {}: slot-0 mismatch after {}({})", i, sig, v
        );
    }
}

#[test]
fn fuzz_amm_matches_solidity() {
    // Random add_liquidity / swap sequences against the mock-backed amm,
    // diffing reserves, total_shares, the shares mapping, success, and logs.
    // Values stay in u64 range so most calls succeed and genuinely exercise
    // the swap math; any divergence (arith, rounding, revert condition) fails.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };
    let mock = sol_creation_bytecode(SOL_MOCK, &solc);
    let gum_amm = gum_creation_bytecode(&read_repo_file("examples/amm.gum"), &solc, true);
    let sol_amm = sol_creation_bytecode(&read_repo_file("examples/solidity/amm.sol"), &solc);

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, mock.clone());
    let gb = deploy(&mut gdb, mock.clone());
    let gamm = deploy(&mut gdb, gum_amm);
    let sa = deploy(&mut sdb, mock.clone());
    let sb = deploy(&mut sdb, mock.clone());
    let samm = deploy(&mut sdb, sol_amm);
    assert_eq!((ga, gb, gamm), (sa, sb, samm), "deploy addresses diverged");

    let init = encode_words("initialize(address,address)", &[word_addr(ga), word_addr(gb)]);
    assert!(call(&mut gdb, gamm, init.clone()).success);
    assert!(call(&mut sdb, samm, init).success);

    // A pool of distinct senders so shares[sender] is keyed on varied
    // addresses, exercising gum_hash_slot across many keys and confirming
    // per-account balances accumulate identically on both compilers.
    let senders: [Address; 4] = [
        Address::from([0x21u8; 20]),
        Address::from([0x22u8; 20]),
        Address::from([0x23u8; 20]),
        Address::from([0x24u8; 20]),
    ];

    let mut rng = Rng(0x1234_5678);
    for i in 0..150 {
        let sender = senders[(rng.next_u64() % senders.len() as u64) as usize];
        let data = if rng.next_u64() % 2 == 0 {
            let a = U256::from(rng.next_u64());
            let b = U256::from(rng.next_u64());
            encode_words("add_liquidity(uint256,uint256)", &[word_u256(a), word_u256(b)])
        } else {
            let token = if rng.next_u64() % 2 == 0 { ga } else { gb };
            let amt = U256::from(rng.next_u64());
            encode_words("swap(address,uint256)", &[word_addr(token), word_u256(amt)])
        };
        let gr = call_from(&mut gdb, sender, gamm, data.clone());
        let sr = call_from(&mut sdb, sender, samm, data);

        assert_eq!(gr.success, sr.success, "iter {}: success mismatch (sender {:?})", i, sender);
        assert_eq!(gr.output, sr.output, "iter {}: output mismatch", i);
        assert_logs_match(&format!("iter {}", i), &gr, &sr);
        for slot in [2u64, 3, 4] {
            assert_eq!(
                storage(&mut gdb, gamm, slot),
                storage(&mut sdb, samm, slot),
                "iter {}: reserve/shares slot {} mismatch", i, slot
            );
        }
        // Diff every sender's mapping slot, not just the current one, a
        // hashing bug could corrupt any key, not only the one just written.
        for s in &senders {
            let mslot = mapping_slot(*s, 5);
            assert_eq!(
                storage_at(&mut gdb, gamm, mslot),
                storage_at(&mut sdb, samm, mslot),
                "iter {}: shares[{:?}] mismatch", i, s
            );
        }
    }
}

// Storage slot of m[id] for a uint256-keyed mapping at base slot < 256.
fn mapping_slot_uint(key: U256, base: u8) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let mut buf = [0u8; 64];
    buf[0..32].copy_from_slice(&key.to_be_bytes::<32>());
    buf[63] = base;
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(&buf);
    k.finalize(&mut out);
    U256::from_be_bytes(out)
}

// keccak256(pad32(len_slot)), the data base of a dynamic storage array.
fn dyn_array_data_base(len_slot: u64) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(&U256::from(len_slot).to_be_bytes::<32>());
    k.finalize(&mut out);
    U256::from_be_bytes(out)
}

const GUM_DYN_ARRAY: &str = include_str!("fixtures/gum_dyn_array.gum");

const SOL_DYN_ARRAY: &str = include_str!("fixtures/sol_dyn_array.sol");

#[test]
fn dynamic_storage_array_matches_solidity() {
    // Dynamic array: length at slot 0, elements at keccak256(0)+i. Diffs the
    // length slot, element slots, getters, success, and OOB/empty reverts
    // (Panic 0x32/0x31) against Solidity across push/set/pop.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(GUM_DYN_ARRAY, &solc, true);
    let sol = sol_creation_bytecode(SOL_DYN_ARRAY, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    let data_base = dyn_array_data_base(0);

    let check = |gdb: &mut Db, sdb: &mut Db| {
        // length slot 0 + first 4 element slots
        assert_eq!(storage(gdb, ga, 0), storage(sdb, sa, 0), "length slot");
        for i in 0u64..4 {
            let s = data_base + U256::from(i);
            assert_eq!(storage_at(gdb, ga, s), storage_at(sdb, sa, s), "element {}", i);
        }
    };

    for (sig, words) in [
        ("push_val(uint256)", vec![word_u256(U256::from(10u64))]),
        ("push_val(uint256)", vec![word_u256(U256::from(20u64))]),
        ("push_val(uint256)", vec![word_u256(U256::from(30u64))]),
        ("set_at(uint256,uint256)", vec![word_u256(U256::from(1u64)), word_u256(U256::from(99u64))]),
        ("pop_val()", vec![]),
    ] {
        let data = encode_words(sig, &words);
        let gr = call(&mut gdb, ga, data.clone());
        let sr = call(&mut sdb, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success", sig);
        assert_eq!(gr.output, sr.output, "{}: output", sig);
        check(&mut gdb, &mut sdb);
    }

    // len() == 2 after 3 pushes and 1 pop.
    assert_eq!(U256::from_be_slice(&call(&mut gdb, ga, encode_words("len()", &[])).output), U256::from(2u64));
    // OOB read (index 5, len 2) reverts identically (Panic 0x32).
    let g = call(&mut gdb, ga, encode_words("get(uint256)", &[word_u256(U256::from(5u64))]));
    let s = call(&mut sdb, sa, encode_words("get(uint256)", &[word_u256(U256::from(5u64))]));
    assert!(!g.success);
    assert_eq!(g.success, s.success, "OOB success");
    assert_eq!(g.output, s.output, "OOB Panic data");
}

const GUM_STORAGE_ARRAY: &str = include_str!("fixtures/gum_storage_array.gum");

const SOL_STORAGE_ARRAY: &str = include_str!("fixtures/sol_storage_array.sol");

#[test]
fn fixed_storage_array_matches_solidity() {
    // uint256[3] occupies slots 0..2, total slot 3. Verifies element i lands
    // at slot base+i (matching Solidity) and that the array reserves its slots
    // so the following field isn't clobbered.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(GUM_STORAGE_ARRAY, &solc, true);
    let sol = sol_creation_bytecode(SOL_STORAGE_ARRAY, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    for (i, v) in [(0u64, 10u64), (2, 20), (1, 5)] {
        let data = encode_words("setit(uint256,uint256)", &[word_u256(U256::from(i)), word_u256(U256::from(v))]);
        assert!(call(&mut gdb, ga, data.clone()).success);
        assert!(call(&mut sdb, sa, data).success);
    }
    // slots 0,1,2 = items; slot 3 = total (10+20+5 = 35)
    for slot in 0u64..=3 {
        assert_eq!(storage(&mut gdb, ga, slot), storage(&mut sdb, sa, slot), "slot {}", slot);
    }
    assert_eq!(storage(&mut gdb, ga, 3), U256::from(35u64), "total");
    let g = call(&mut gdb, ga, encode_words("getit(uint256)", &[word_u256(U256::from(2u64))]));
    assert_eq!(U256::from_be_slice(&g.output), U256::from(20u64));

    // Out-of-bounds (index 3 into a length-3 array) must revert on both, with
    // identical Panic(0x32) reason data (gum compiled with --rich-reverts).
    for sig_words in [
        ("getit(uint256)", vec![word_u256(U256::from(3u64))]),
        ("setit(uint256,uint256)", vec![word_u256(U256::from(9u64)), word_u256(U256::from(1u64))]),
    ] {
        let data = encode_words(sig_words.0, &sig_words.1);
        let gr = call(&mut gdb, ga, data.clone());
        let sr = call(&mut sdb, sa, data);
        assert!(!gr.success, "{}: gum should revert OOB", sig_words.0);
        assert_eq!(gr.success, sr.success, "{}: OOB success mismatch", sig_words.0);
        assert_eq!(gr.output, sr.output, "{}: OOB revert data mismatch (Panic 0x32)", sig_words.0);
    }
}

#[test]
fn once_function_reverts_on_second_call() {
    // token's export once fn initialize must run once and revert thereafter.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/token.gum"), &solc, false);
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let a = deploy(&mut db, gum);
    let init = encode("initialize(uint256)", &[U256::from(1000u64)]);
    assert!(call(&mut db, a, init.clone()).success, "first initialize should succeed");
    assert!(!call(&mut db, a, init).success, "second initialize must revert (once)");
}

#[test]
fn erc721_matches_solidity() {
    // gum's erc721 port diffed against the verbatim OpenZeppelin v5.1 ERC721
    // (flattened, deployed via ERC721Mock). Mirrors OZ's storage order exactly,
    // name(0) symbol(1) _owners(2) _balances(3) _tokenApprovals(4)
    // _operatorApprovals(5), and drives mint / approve / setApprovalForAll /
    // transferFrom plus OZ's custom-error revert paths, diffing return data,
    // success, and every touched storage slot against the audited bytecode.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/erc721.gum"), &solc, true);
    let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/erc721.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let alice = deployer();
    let bob = Address::from([0x81u8; 20]);
    let op = Address::from([0x82u8; 20]);
    let id = U256::from(42u64);

    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (alice, "mint(address,uint256)", vec![word_addr(alice), word_u256(id)]),
        (alice, "approve(address,uint256)", vec![word_addr(op), word_u256(id)]),
        (alice, "setApprovalForAll(address,bool)", vec![word_addr(op), word_u256(U256::from(1u64))]),
        (alice, "transferFrom(address,address,uint256)", vec![word_addr(alice), word_addr(bob), word_u256(id)]),
        // Now bob owns the token. These all revert, and each must revert with
        // OZ's exact custom error (selector + args), so the outputs must match.
        (alice, "balanceOf(address)", vec![word_addr(Address::ZERO)]), // ERC721InvalidOwner(0)
        (alice, "ownerOf(uint256)", vec![word_u256(U256::from(999u64))]), // ERC721NonexistentToken(999)
        (op, "transferFrom(address,address,uint256)", vec![word_addr(bob), word_addr(alice), word_u256(id)]), // ERC721InsufficientApproval(op, 42)
        (alice, "mint(address,uint256)", vec![word_addr(alice), word_u256(id)]), // ERC721InvalidSender(0): already minted
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert_eq!(gr.output, sr.output, "{}: output/revert mismatch", sig);
        // _owners[id] (slot2), _tokenApprovals[id] (slot4)
        assert_eq!(storage_at(&mut gdb, ga, mapping_slot_uint(id, 2)), storage_at(&mut sdb, sa, mapping_slot_uint(id, 2)), "{}: owners[id]", sig);
        assert_eq!(storage_at(&mut gdb, ga, mapping_slot_uint(id, 4)), storage_at(&mut sdb, sa, mapping_slot_uint(id, 4)), "{}: approvals[id]", sig);
        // _balances (slot3) for both parties
        for acct in [alice, bob] {
            let s = mapping_slot(acct, 3);
            assert_eq!(storage_at(&mut gdb, ga, s), storage_at(&mut sdb, sa, s), "{}: balance[{:?}]", sig, acct);
        }
        // _operatorApprovals[alice][op] (nested, slot5)
        let s = nested_mapping_slot(alice, op, 5);
        assert_eq!(storage_at(&mut gdb, ga, s), storage_at(&mut sdb, sa, s), "{}: operator approval", sig);
    }

    // End state: bob owns the token, each has balance as expected.
    assert_eq!(U256::from_be_slice(&call(&mut gdb, ga, encode_words("ownerOf(uint256)", &[word_u256(id)])).output), U256::from_be_slice(bob.as_slice()));
}

// A bytes4 argument rides the wire left-aligned: its 4 bytes in the high end
// of the word, the rest zero. This is how Solidity ABI-encodes bytes4.
fn word_bytes4(id: u32) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[..4].copy_from_slice(&id.to_be_bytes());
    w
}

#[test]
fn abi_encode_matches_solidity() {
    // Abi.encode / Abi.encode_packed hashed with keccak256 must be byte-for-byte
    // Solidity's abi.encode / abi.encodePacked: static values (uint/address/
    // bytes32), and the packed/standard forms over dynamic string content.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "use gum.defaults.hashable\nuse gum.defaults.String\n\ncontract H:\n    export fn e_static(u256 a, Account b, b32 c) -> u256:\n        return keccak256(Abi.encode(a, b, c))\n\n    export fn p_static(u256 a, Account b) -> u256:\n        return keccak256(Abi.encode_packed(a, b))\n\n    export fn e_str() -> u256:\n        return keccak256(Abi.encode(\"hello\", \"world\"))\n\n    export fn p_str() -> u256:\n        return keccak256(Abi.encode_packed(\"hello\", \"world\"))\n";
    let sol_src = "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\ncontract H {\n    function e_static(uint256 a, address b, bytes32 c) external pure returns (bytes32) { return keccak256(abi.encode(a,b,c)); }\n    function p_static(uint256 a, address b) external pure returns (bytes32) { return keccak256(abi.encodePacked(a,b)); }\n    function e_str() external pure returns (bytes32) { return keccak256(abi.encode(\"hello\",\"world\")); }\n    function p_str() external pure returns (bytes32) { return keccak256(abi.encodePacked(\"hello\",\"world\")); }\n}\n";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let sa = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let addr = Address::from([0x42u8; 20]);
    let mut c = [0u8; 32];
    c[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let checks: Vec<(&str, Vec<[u8; 32]>)> = vec![
        ("e_static(uint256,address,bytes32)", vec![word_u256(U256::from(123u64)), word_addr(addr), c]),
        ("p_static(uint256,address)", vec![word_u256(U256::from(123u64)), word_addr(addr)]),
        ("e_str()", vec![]),
        ("p_str()", vec![]),
    ];
    for (sig, words) in &checks {
        let data = encode_words(sig, words);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "{}: success mismatch", sig);
        assert!(g.success, "{}: gum reverted", sig);
        assert_eq!(g.output, s.output, "{}: hash mismatch", sig);
    }
}

#[test]
fn fixed_bytes_round_trip() {
    // The bN family: b32 fills the word (identity in and out); a sub-word b4
    // rides the wire left-aligned, is carried right-aligned internally, and is
    // laid back out left-aligned. Also checks a b4 compares against a plain
    // literal, the interface-id use.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "use gum.defaults.hashable\n\ncontract Bt:\n    export fn echo32(b32 x) -> b32:\n        return x\n    export fn echo4(b4 x) -> b4:\n        return x\n    export fn is165(b4 x) -> bool:\n        return x == 0x01ffc9a7\n";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let a = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    // b32: a full 32-byte word round-trips byte-identically.
    let w = [0xABu8; 32];
    let out = call(&mut db, a, encode_words("echo32(bytes32)", &[w])).output;
    assert_eq!(&out[..32], &w, "b32 round-trips");

    // b4: left-aligned on the wire both directions.
    let b4 = word_bytes4(0x01ffc9a7);
    let out = call(&mut db, a, encode_words("echo4(bytes4)", &[b4])).output;
    assert_eq!(&out[..32], &b4, "b4 round-trips left-aligned");

    // b4 vs literal.
    let t = call(&mut db, a, encode_words("is165(bytes4)", &[word_bytes4(0x01ffc9a7)])).output;
    assert_eq!(U256::from_be_slice(&t), U256::from(1u8), "is165 true for 0x01ffc9a7");
    let f = call(&mut db, a, encode_words("is165(bytes4)", &[word_bytes4(0xdeadbeef)])).output;
    assert_eq!(U256::from_be_slice(&f), U256::from(0u8), "is165 false otherwise");
}

#[test]
fn erc721_supports_interface_matches_solidity() {
    // ERC165: gum's supportsInterface(bytes4) must answer identically to the
    // verbatim OZ ERC721 for the three interface ids it claims (ERC165,
    // IERC721, IERC721Metadata) and reject anything else. This also exercises
    // the bytes4 calldata type end to end, selector 0x01ffc9a7 and the
    // left-aligned wire decode.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/erc721.gum"), &solc, true);
    let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/erc721.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    // ERC165, IERC721, IERC721Metadata (all true), then two ids that are not
    // supported (false), including the 0xffffffff ERC165 explicitly forbids.
    for id in [0x01ffc9a7u32, 0x80ac58cd, 0x5b5e139f, 0xffffffff, 0x12345678] {
        let data = encode_words("supportsInterface(bytes4)", &[word_bytes4(id)]);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "supportsInterface(0x{:08x}): success mismatch", id);
        assert_eq!(g.output, s.output, "supportsInterface(0x{:08x}): answer mismatch", id);
    }
}

#[test]
fn erc721_token_uri_matches_solidity() {
    // tokenURI = baseURI + tokenId.to_string(). Diffs gum's version (String
    // concat over the itoa) against the Solidity twin for several ids, and
    // confirms a nonexistent token reverts ERC721NonexistentToken identically.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/erc721.gum"), &solc, true);
    let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/erc721.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    let alice = deployer();

    for id in [0u64, 7, 42, 123456789] {
        let m = encode_words("mint(address,uint256)", &[word_addr(alice), word_u256(U256::from(id))]);
        call(&mut gdb, ga, m.clone());
        call(&mut sdb, sa, m);
        let data = encode_words("tokenURI(uint256)", &[word_u256(U256::from(id))]);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "tokenURI({}): success mismatch", id);
        assert_eq!(g.output, s.output, "tokenURI({}): uri mismatch", id);
    }

    // Nonexistent token: both revert ERC721NonexistentToken(999), same bytes.
    let data = encode_words("tokenURI(uint256)", &[word_u256(U256::from(999u64))]);
    let g = call(&mut gdb, ga, data.clone());
    let s = call(&mut sdb, sa, data);
    assert!(!g.success && !s.success, "tokenURI(nonexistent) should revert on both");
    assert_eq!(g.output, s.output, "tokenURI(nonexistent): revert data mismatch");
}

#[test]
fn to_string_result_compares_as_a_string() {
    // Regression (surfaced by gumc test): a var s = n.to_string() local must
    // type as String, so s == "..." lowers to string equality, not a pointer
    // compare of two distinct allocations.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "use gum.defaults.hashable\nuse gum.defaults.String\n\ncontract C:\n    export fn is7(u256 n) -> bool:\n        var s = n.to_string()\n        return s == \"7\"\n";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let a = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let yes = call(&mut db, a, encode_words("is7(uint256)", &[word_u256(U256::from(7u64))])).output;
    assert_eq!(U256::from_be_slice(&yes), U256::from(1u8), "7.to_string() == \"7\"");
    let no = call(&mut db, a, encode_words("is7(uint256)", &[word_u256(U256::from(8u64))])).output;
    assert_eq!(U256::from_be_slice(&no), U256::from(0u8), "8.to_string() != \"7\"");
}

#[test]
fn uint_to_string_produces_decimal() {
    // The <uint>.to_string() itoa: every value must render as its exact decimal
    // ASCII, zero as "0" (not empty), and large values digit-for-digit. Asserts
    // the returned ABI string directly, so it needs no OZ Strings dependency.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "use gum.defaults.hashable\nuse gum.defaults.String\n\ncontract Stringify:\n    export fn stringify(u256 x) -> String:\n        return x.to_string()\n";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let a = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    let decode = |out: &[u8]| -> String {
        let len: usize = U256::from_be_slice(&out[32..64]).to();
        String::from_utf8_lossy(&out[64..64 + len]).into_owned()
    };
    for (n, expect) in [
        (U256::ZERO, "0"),
        (U256::from(7u64), "7"),
        (U256::from(42u64), "42"),
        (U256::from(255u64), "255"),
        (U256::from(1000u64), "1000"),
        (U256::from(123456789u64), "123456789"),
        // 2^128, well past one word of decimal digits
        (U256::from(1u128) << 128, "340282366920938463463374607431768211456"),
        (U256::MAX, "115792089237316195423570985008687907853269984665640564039457584007913129639935"),
    ] {
        let out = call(&mut db, a, encode_words("stringify(uint256)", &[word_u256(n)])).output;
        assert_eq!(decode(&out), expect, "stringify({})", n);
    }
}

// Storage slot of nested m[k1][k2] at base slot < 256: keccak(k2 . keccak(k1 . base)).
fn nested_mapping_slot(k1: Address, k2: Address, base: u8) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let inner = mapping_slot(k1, base); // keccak(k1 . base)
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(k2.as_slice());
    buf[32..64].copy_from_slice(&inner.to_be_bytes::<32>());
    let mut kk = Keccak::v256();
    let mut out = [0u8; 32];
    kk.update(&buf);
    kk.finalize(&mut out);
    U256::from_be_bytes(out)
}

const GUM_NESTED_BRACKET: &str = include_str!("fixtures/gum_nested_bracket.gum");

const SOL_NESTED_BRACKET: &str = include_str!("fixtures/sol_nested_bracket.sol");

#[test]
fn nested_mapping_bracket_sugar_matches_solidity() {
    // The m[a][b] index form (read, write, and read-modify-write in one
    // statement) must match Solidity's nested layout exactly.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(GUM_NESTED_BRACKET, &solc, true);
    let sol = sol_creation_bytecode(SOL_NESTED_BRACKET, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    let a = Address::from([0x41u8; 20]);
    let b = Address::from([0x42u8; 20]);
    let slot = nested_mapping_slot(a, b, 0);

    for (sig, words) in [
        ("setv(address,address,uint256)", vec![word_addr(a), word_addr(b), word_u256(U256::from(7u64))]),
        ("incv(address,address,uint256)", vec![word_addr(a), word_addr(b), word_u256(U256::from(5u64))]),
    ] {
        let data = encode_words(sig, &words);
        let gr = call(&mut gdb, ga, data.clone());
        let sr = call(&mut sdb, sa, data);
        assert_eq!(gr.success, sr.success, "{}", sig);
        assert_eq!(storage_at(&mut gdb, ga, slot), storage_at(&mut sdb, sa, slot), "{}: nested slot", sig);
    }
    let g = call(&mut gdb, ga, encode_words("getv(address,address)", &[word_addr(a), word_addr(b)]));
    assert_eq!(U256::from_be_slice(&g.output), U256::from(12u64), "7 + 5 via nested RMW");
}

#[test]
fn erc20_with_allowances_matches_solidity() {
    // Full ERC20 including the nested allowances mapping, the codegen path
    // token/amm never exercised. Drives init / approve / transfer_from across
    // multiple accounts and diffs balances, the nested allowance slot, and
    // total_supply against the Solidity twin at every step.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/erc20.gum"), &solc, true);
    let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/erc20.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    assert_eq!(ga, sa, "deploy addresses diverged");

    let owner = deployer();
    let spender = Address::from([0x31u8; 20]);
    let recipient = Address::from([0x32u8; 20]);

    // (caller, sig, words)
    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (owner, "init(uint256)", vec![word_u256(U256::from(1_000_000u64))]),
        (owner, "approve(address,uint256)", vec![word_addr(spender), word_u256(U256::from(400u64))]),
        (spender, "transferFrom(address,address,uint256)", vec![word_addr(owner), word_addr(recipient), word_u256(U256::from(300u64))]),
        (owner, "transfer(address,uint256)", vec![word_addr(recipient), word_u256(U256::from(50u64))]),
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert!(gr.success, "{} reverted on gum", sig);
        assert_eq!(gr.output, sr.output, "{}: return mismatch", sig);

        // total_supply (slot 2)
        assert_eq!(storage(&mut gdb, ga, 2), storage(&mut sdb, sa, 2), "{}: total_supply", sig);
        // balances (slot 0)
        let bl = mapping_slot(owner, 0);
        assert_eq!(storage_at(&mut gdb, ga, bl), storage_at(&mut sdb, sa, bl), "{}: balance[owner]", sig);
        let br = mapping_slot(recipient, 0);
        assert_eq!(storage_at(&mut gdb, ga, br), storage_at(&mut sdb, sa, br), "{}: balance[recipient]", sig);
        // allowances (slot 1)
        let al = nested_mapping_slot(owner, spender, 1);
        assert_eq!(storage_at(&mut gdb, ga, al), storage_at(&mut sdb, sa, al), "{}: allowance[owner][spender]", sig);
    }

    // Concrete end-state: recipient got 300 (transferFrom) + 50 (transfer),
    // allowance dropped 400 -> 100.
    assert_eq!(storage_at(&mut gdb, ga, mapping_slot(recipient, 0)), U256::from(350u64), "recipient balance");
    assert_eq!(storage_at(&mut gdb, ga, nested_mapping_slot(owner, spender, 1)), U256::from(100u64), "remaining allowance");
}

const GUM_PACKED_STRUCT: &str = include_str!("fixtures/gum_packed_struct.gum");

const SOL_PACKED_STRUCT: &str = include_str!("fixtures/sol_packed_struct.sol");

#[test]
fn packed_struct_slot_layout_matches_solidity() {
    // Two u128s share one slot inside a struct-in-mapping. The RAW slot word
    // must match Solidity byte-for-byte, i.e. gum packs the first field into
    // the low-order bytes like solc, not the high-order bytes.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(GUM_PACKED_STRUCT, &solc, true);
    let sol = sol_creation_bytecode(SOL_PACKED_STRUCT, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    let k = Address::from([0x71u8; 20]);

    let data = encode_words(
        "setp(address,uint128,uint128)",
        &[word_addr(k), word_u256(U256::from(0xAAAu64)), word_u256(U256::from(0xBBBu64))],
    );
    assert!(call(&mut gdb, ga, data.clone()).success);
    assert!(call(&mut sdb, sa, data).success);

    let slot = mapping_slot(k, 0);
    assert_eq!(
        storage_at(&mut gdb, ga, slot),
        storage_at(&mut sdb, sa, slot),
        "packed struct slot word differs from Solidity"
    );
    // And each field reads back correctly on gum.
    let lo = call(&mut gdb, ga, encode_words("getlo(address)", &[word_addr(k)]));
    let hi = call(&mut gdb, ga, encode_words("gethi(address)", &[word_addr(k)]));
    assert_eq!(U256::from_be_slice(&lo.output), U256::from(0xAAAu64), "lo");
    assert_eq!(U256::from_be_slice(&hi.output), U256::from(0xBBBu64), "hi");
}

#[test]
fn vault_struct_in_mapping_matches_solidity() {
    // Struct-valued mapping: stakes[who].{amount,since} occupy two consecutive
    // storage slots at the mapping hash. Diffs the struct fields (base slot and
    // base+1), total, success, and revert data against Solidity, including a
    // struct-field read-modify-write and an insufficient-stake revert.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/vault.gum"), &solc, true);
    let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/vault.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let a = deployer();
    let b = Address::from([0x61u8; 20]);

    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (a, "deposit(uint256,uint256)", vec![word_u256(U256::from(100u64)), word_u256(U256::from(5000u64))]),
        (a, "deposit(uint256,uint256)", vec![word_u256(U256::from(50u64)), word_u256(U256::from(6000u64))]),
        (b, "deposit(uint256,uint256)", vec![word_u256(U256::from(200u64)), word_u256(U256::from(7000u64))]),
        (a, "withdraw(uint256)", vec![word_u256(U256::from(30u64))]),
        (a, "withdraw(uint256)", vec![word_u256(U256::from(999u64))]), // reverts (insufficient)
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert_eq!(gr.output, sr.output, "{}: output/revert mismatch", sig);
        assert_eq!(storage(&mut gdb, ga, 0), storage(&mut sdb, sa, 0), "{}: total", sig);
        // stakes[acct].amount at mapping_slot(acct,1); .since at +1.
        for acct in [a, b] {
            let base = mapping_slot(acct, 1);
            assert_eq!(storage_at(&mut gdb, ga, base), storage_at(&mut sdb, sa, base), "{}: {:?}.amount", sig, acct);
            let since = base + U256::from(1u64);
            assert_eq!(storage_at(&mut gdb, ga, since), storage_at(&mut sdb, sa, since), "{}: {:?}.since", sig, acct);
        }
    }

    // End state: a staked 150 then withdrew 30 -> 120; since=6000; total=320.
    let ga_base = mapping_slot(a, 1);
    assert_eq!(storage_at(&mut gdb, ga, ga_base), U256::from(120u64), "a.amount");
    assert_eq!(storage_at(&mut gdb, ga, ga_base + U256::from(1u64)), U256::from(6000u64), "a.since");
    assert_eq!(storage(&mut gdb, ga, 0), U256::from(320u64), "total");
}

#[test]
fn fuzz_erc20_matches_solidity() {
    // Complex stress: random transfer/approve/transfer_from across a pool of
    // accounts, from random callers, diffing every balance, every allowance
    // pair, total_supply, success and revert data against Solidity each step.
    // Hammers the nested-mapping + checked-arithmetic paths under many states,
    // including insufficient-balance/allowance reverts (which must match).
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };
    let gum = gum_creation_bytecode(&read_repo_file("examples/erc20.gum"), &solc, true);
    let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/erc20.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let accts: [Address; 4] = [
        deployer(),
        Address::from([0x51u8; 20]),
        Address::from([0x52u8; 20]),
        Address::from([0x53u8; 20]),
    ];
    // Owner (deployer) starts with the whole supply.
    let init = encode_words("init(uint256)", &[word_u256(U256::from(1_000_000u64))]);
    assert!(call(&mut gdb, ga, init.clone()).success);
    assert!(call(&mut sdb, sa, init).success);

    let mut rng = Rng(0xe2c0_1234);
    for i in 0..250 {
        let caller = accts[(rng.next_u64() % 4) as usize];
        let a1 = accts[(rng.next_u64() % 4) as usize];
        let a2 = accts[(rng.next_u64() % 4) as usize];
        let amt = U256::from(rng.next_u64() % 400_000); // spans success and revert
        let data = match rng.next_u64() % 3 {
            0 => encode_words("transfer(address,uint256)", &[word_addr(a1), word_u256(amt)]),
            1 => encode_words("approve(address,uint256)", &[word_addr(a1), word_u256(amt)]),
            _ => encode_words("transferFrom(address,address,uint256)", &[word_addr(a1), word_addr(a2), word_u256(amt)]),
        };
        let gr = call_from(&mut gdb, caller, ga, data.clone());
        let sr = call_from(&mut sdb, caller, sa, data);
        assert_eq!(gr.success, sr.success, "iter {}: success mismatch", i);
        assert_eq!(gr.output, sr.output, "iter {}: output/revert mismatch", i);
        assert_eq!(storage(&mut gdb, ga, 2), storage(&mut sdb, sa, 2), "iter {}: total_supply", i);
        for acct in accts {
            let s = mapping_slot(acct, 0);
            assert_eq!(storage_at(&mut gdb, ga, s), storage_at(&mut sdb, sa, s), "iter {}: balance[{:?}]", i, acct);
        }
        for owner in accts {
            for spender in accts {
                let s = nested_mapping_slot(owner, spender, 1);
                assert_eq!(storage_at(&mut gdb, ga, s), storage_at(&mut sdb, sa, s), "iter {}: allowance[{:?}][{:?}]", i, owner, spender);
            }
        }
    }
}

// A memory-allocator torture contract: two arrays (a, b) kept live while a
// fresh array (tmp) is allocated every loop iteration. If allocate_memory ever
// handed back overlapping regions, tmp's writes would clobber a or b and the
// running checksum would diverge from the closed-form expected value. The
// while-loop count is fuzzed so the number of interleaved allocations varies.
const MEM_STRESS: &str = include_str!("fixtures/mem_stress.gum");

fn mem_stress_expected(rounds: u64) -> U256 {
    // Runtime contents (tmp=[i,i,i,i]) and runtime indices (a[i%4], b[i%4],
    // tmp[i%4]) defeat scalarization, forcing real allocations that must not
    // overlap. per iter: tmp[i%4]=i + a[i%4] + b[i%4].
    let a = [1u64, 2, 3, 4];
    let b = [10u64, 20, 30, 40];
    let mut acc = U256::ZERO;
    for i in 0..rounds {
        let m = (i % 4) as usize;
        acc += U256::from(i) + U256::from(a[m]) + U256::from(b[m]);
    }
    acc
}

#[test]
fn memory_allocator_stress_holds() {
    // Confirms gum's bump allocator never overlaps live objects, across a
    // fuzzed number of interleaved allocations. A self-contained correctness
    // check (closed-form expected value), no Solidity reference needed.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut gdb, gum_creation_bytecode(MEM_STRESS, &solc, false));

    let mut rng = Rng(0x0a11_0c);
    for _ in 0..40 {
        let rounds = rng.next_u64() % 128;
        let r = call(&mut gdb, addr, encode("mem_stress(uint256)", &[U256::from(rounds)]));
        assert!(r.success, "mem_stress reverted at rounds={}", rounds);
        let got = U256::from_be_slice(&r.output);
        assert_eq!(got, mem_stress_expected(rounds), "memory corruption at rounds={}", rounds);
    }
}

#[test]
fn custom_errors_revert_with_matching_selector() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum_src = "
enum Errors:
    InsufficientBalance(u256 required, u256 available)

contract TestError:
    export fn test_revert():
        revert Errors.InsufficientBalance(100, 50)
".trim();
    
    let sol_src = "
        pragma solidity ^0.8.0;
        
        error InsufficientBalance(uint256 required, uint256 available);
        
        contract TestError {
            function test_revert() public pure {
                revert InsufficientBalance(100, 50);
            }
        }
    ";
    
    let gum_code = gum_creation_bytecode(gum_src, &solc, false);
    let sol_code = sol_creation_bytecode(sol_src, &solc);
    
    let mut gum_db: Db = CacheDB::new(EmptyDB::default());
    let gum_addr = deploy(&mut gum_db, gum_code);
    let mut sol_db: Db = CacheDB::new(EmptyDB::default());
    let sol_addr = deploy(&mut sol_db, sol_code);
    
    // selector for test_revert() = 0x6e78dd6d
    let calldata = hex::decode("6e78dd6d").unwrap();
    
    let gum_res = call(&mut gum_db, gum_addr, calldata.clone());
    let sol_res = call(&mut sol_db, sol_addr, calldata);
    
    assert!(!gum_res.success, "gum did not revert!");
    let gum_revert_data = gum_res.output;
    
    assert!(!sol_res.success, "solidity did not revert!");
    let sol_revert_data = sol_res.output;
    
    assert_eq!(gum_revert_data, sol_revert_data, "Gum custom error data must exactly match Solidity");
}

#[test]
fn custom_error_with_dynamic_string_arg_matches_solidity() {
    // A custom error carrying a dynamic String alongside a static u256.
    // The revert data must be ABI-encoded head/tail (offset word for the
    // string, inline word for the uint, then the string's length+bytes in the
    // tail), byte-for-byte identical to Solidity's revert Bad(reason, 7).
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "
use gum.defaults.String

enum Errors:
    Bad(String reason, u256 code)

contract App:
    export fn check(String reason):
        revert Errors.Bad(reason, 7)
".trim();

    let sol_src = "
        // SPDX-License-Identifier: MIT
        pragma solidity 0.8.36;
        error Bad(string reason, uint256 code);
        contract C {
            function check(string calldata reason) external pure {
                revert Bad(reason, 7);
            }
        }
    ";

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let reason = b"balance below required minimum threshold";
    let data = encode_abi("check(string)", &[Arg::Dyn(reason)]);
    let gr = call(&mut gdb, gaddr, data.clone());
    let sr = call(&mut sdb, saddr, data);

    assert!(!gr.success && !sr.success, "both must revert (gum={}, sol={})", gr.success, sr.success);
    assert_eq!(gr.output, sr.output, "gum dynamic custom-error data must match Solidity byte-for-byte");
}

// An ABI argument: either a static 32-byte word or a dynamic byte string.
// How a string/bytes return arrives on the wire: a head word holding the
// offset (always 32, it is the only return value), a length word, then the
fn encode_abi_return_string(bytes: &[u8]) -> Vec<u8> {
    let mut v = U256::from(32u64).to_be_bytes::<32>().to_vec();
    v.extend_from_slice(&U256::from(bytes.len()).to_be_bytes::<32>());
    v.extend_from_slice(bytes);
    let pad = (32 - (bytes.len() % 32)) % 32;
    v.extend(vec![0u8; pad]);
    v
}

enum Arg<'a> {
    Static([u8; 32]),
    Dyn(&'a [u8]),
    // A T[]: a count word then one word per element, however narrow the
    // element type is, the ABI never packs an array the way memory or storage
    // does.
    Arr(&'a [U256]),
}

// Encodes selector + head + tail for a mix of static and dynamic args,
// exactly as Solidity lays out a call (heads in order; each dynamic head is
// the byte offset from the start of the head region to its length word).
fn encode_abi(sig: &str, args: &[Arg]) -> Vec<u8> {
    let head_size = args.len() * 32;
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for a in args {
        match a {
            Arg::Static(w) => head.extend_from_slice(w),
            Arg::Dyn(bytes) => {
                let off = head_size + tail.len();
                head.extend_from_slice(&U256::from(off).to_be_bytes::<32>());
                tail.extend_from_slice(&U256::from(bytes.len()).to_be_bytes::<32>());
                tail.extend_from_slice(bytes);
                let pad = (32 - (bytes.len() % 32)) % 32;
                tail.extend(vec![0u8; pad]);
            }
            Arg::Arr(words) => {
                let off = head_size + tail.len();
                head.extend_from_slice(&U256::from(off).to_be_bytes::<32>());
                tail.extend_from_slice(&U256::from(words.len()).to_be_bytes::<32>());
                for w in *words {
                    tail.extend_from_slice(&w.to_be_bytes::<32>());
                }
            }
        }
    }
    let mut v = selector(sig).to_vec();
    v.extend_from_slice(&head);
    v.extend_from_slice(&tail);
    v
}

#[test]
fn mixed_static_and_dynamic_abi_args_match_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_ABI_MIX, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_ABI_MIX, &solc));

    let a = b"first string argument, over thirty-two bytes long to force padding";
    let b = b"B";

    // pick(which, a, b) with both which=0 (returns a) and which=1 (returns b):
    // proves head/tail decoding of two dynamic args past a leading static one,
    // and dynamic re-encoding of whichever was chosen.
    for which in [0u64, 1u64] {
        let data = encode_abi(
            "pick(uint256,string,string)",
            &[Arg::Static(word_u256(U256::from(which))), Arg::Dyn(a), Arg::Dyn(b)],
        );
        let gr = call(&mut gdb, gaddr, data.clone());
        let sr = call(&mut sdb, saddr, data);
        assert!(gr.success && sr.success, "pick({}) reverted (gum={}, sol={})", which, gr.success, sr.success);
        assert_eq!(gr.output, sr.output, "pick({}) output must match Solidity", which);
    }

    // total_len(a, b): two dynamic args, static return, proves each dynamic
    // arg's length word is read from the right tail location.
    let data = encode_abi("total_len(string,string)", &[Arg::Dyn(a), Arg::Dyn(b)]);
    let gr = call(&mut gdb, gaddr, data.clone());
    let sr = call(&mut sdb, saddr, data);
    assert!(gr.success && sr.success, "total_len reverted");
    assert_eq!(gr.output, sr.output, "total_len output must match Solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(a.len() + b.len()), "total_len value");
}

#[test]
fn constructor_decodes_args_and_initializes_storage() {
    // A new method on the contract singleton is the deploy-time
    // constructor: its args are ABI-encoded and appended to the creation code,
    // decoded from the tail, and run against storage before the runtime code
    // is returned. Differential vs a Solidity twin with the same layout.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "
use gum.defaults.Account
use gum.defaults.Message

contract State:
    Account owner
    u256 total_supply

    fn new(u256 initial_supply):
        State.owner = Message.sender()
        State.total_supply = initial_supply

    export fn supply() -> u256:
        return State.total_supply

    export fn owner_of() -> Account:
        return State.owner
".trim();

    let sol_src = "
        pragma solidity ^0.8.0;
        contract State {
            address owner;
            uint256 total_supply;
            constructor(uint256 initial_supply) {
                owner = msg.sender;
                total_supply = initial_supply;
            }
            function supply() public view returns (uint256) { return total_supply; }
            function owner_of() public view returns (address) { return owner; }
        }
    ";

    let supply_val = U256::from(1_000_000u64);
    let mut gum_code = gum_creation_bytecode(gum_src, &solc, false);
    gum_code.extend_from_slice(&supply_val.to_be_bytes::<32>());
    let mut sol_code = sol_creation_bytecode(sol_src, &solc);
    sol_code.extend_from_slice(&supply_val.to_be_bytes::<32>());

    let mut gum_db: Db = CacheDB::new(EmptyDB::default());
    let gum_addr = deploy(&mut gum_db, gum_code);
    let mut sol_db: Db = CacheDB::new(EmptyDB::default());
    let sol_addr = deploy(&mut sol_db, sol_code);

    // Storage was populated at construction time (no post-deploy init call).
    assert_eq!(storage(&mut gum_db, gum_addr, 1), supply_val, "gum total_supply slot");
    assert_eq!(
        storage(&mut gum_db, gum_addr, 0),
        U256::from_be_bytes(word_addr(deployer())),
        "gum owner slot = deployer",
    );

    // Runtime getters agree with the Solidity twin.
    for sig in ["supply()", "owner_of()"] {
        let g = call(&mut gum_db, gum_addr, selector(sig).to_vec());
        let s = call(&mut sol_db, sol_addr, selector(sig).to_vec());
        assert!(g.success && s.success, "{} call failed", sig);
        assert_eq!(g.output, s.output, "{} output must match Solidity", sig);
    }
}

// ABI-encodes a single dynamic string/bytes return value the way the EVM
// returns it: offset(32), length, then zero-padded data.
fn abi_encode_string(s: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
    v.extend_from_slice(&U256::from(s.len()).to_be_bytes::<32>());
    v.extend_from_slice(s);
    let pad = (32 - (s.len() % 32)) % 32;
    v.extend(vec![0u8; pad]);
    v
}

#[test]
fn string_literal_return_matches_solidity() {
    // A string literal is a first-class String: returning it ABI-encodes
    // byte-for-byte like Solidity returning the same string literal.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum_src = "\
use gum.defaults.String

contract App:
    export fn greet() -> String:
        return \"hello, gum world\"
";
    let sol_src = "\
// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract G { function greet() external pure returns (string memory) { return \"hello, gum world\"; } }
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));
    let g = call(&mut gdb, gaddr, selector("greet()").to_vec());
    let s = call(&mut sdb, saddr, selector("greet()").to_vec());
    assert!(g.success && s.success, "greet() reverted");
    assert_eq!(g.output, s.output, "string literal return must match Solidity");
    assert_eq!(g.output, abi_encode_string(b"hello, gum world"));
}

#[test]
fn fstring_return_is_valid_string() {
    // An f-string is a String too: its result ABI-encodes as the concatenated
    // text with a proper length prefix.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum_src = "\
use gum.defaults.String

contract App:
    export fn label(u256 n) -> String:
        return f\"n={n}!\"
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let data = encode("label(uint256)", &[U256::from(42u64)]);
    let g = call(&mut gdb, gaddr, data);
    assert!(g.success, "label() reverted");
    assert_eq!(g.output, abi_encode_string(b"n=42!"), "f-string must encode as the concatenated text");
}

#[test]
fn custom_error_with_string_literal_arg_matches_solidity() {
    // Passing a string literal to a custom error now works (literals are
    // Strings), and encodes byte-for-byte like Solidity.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum_src = "\
use gum.defaults.String

enum Errors:
    Denied(String reason)

contract App:
    export fn go():
        revert Errors.Denied(\"not allowed\")
";
    let sol_src = "\
// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
error Denied(string reason);
contract C { function go() external pure { revert Denied(\"not allowed\"); } }
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));
    let g = call(&mut gdb, gaddr, selector("go()").to_vec());
    let s = call(&mut sdb, saddr, selector("go()").to_vec());
    assert!(!g.success && !s.success, "both must revert");
    assert_eq!(g.output, s.output, "string-literal error arg must match Solidity");
}

const GUM_STR_OPS: &str = include_str!("fixtures/gum_str_ops.gum");

const SOL_STR_OPS: &str = include_str!("fixtures/sol_str_ops.sol");

#[test]
fn string_ops_match_solidity() {
    // concat / == / != / indexing / slice, against a Solidity twin. Cases
    // deliberately cross the 32-byte word boundary and include empty strings,
    // since the equality helper compares whole words then a masked remainder.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STR_OPS, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STR_OPS, &solc));

    let long_a: &[u8] = b"the quick brown fox jumps over the lazy dog and keeps running";
    let cases: &[(&[u8], &[u8])] = &[
        (b"", b""),
        (b"a", b"a"),
        (b"a", b"b"),
        (b"abc", b"abd"),
        (long_a, long_a),
        // Differ only in the trailing partial word, the masked-remainder path.
        (b"0123456789012345678901234567890123456789", b"012345678901234567890123456789012345678X"),
        // Differ only in a full word, the whole-word path.
        (b"X123456789012345678901234567890123456789", b"0123456789012345678901234567890123456789"),
        // Same prefix, different length.
        (b"abc", b"abcd"),
        (b"hello ", b"world"),
    ];

    for (a, b) in cases {
        for sig in ["cat(string,string)", "same(string,string)", "differs(string,string)"] {
            let data = encode_abi(sig, &[Arg::Dyn(a), Arg::Dyn(b)]);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(g.success, s.success, "{} success differs for {:?}/{:?}", sig, a, b);
            assert_eq!(g.output, s.output, "{} output differs for {:?}/{:?}", sig, a, b);
        }
    }

    // Indexing: every in-bounds byte, plus an out-of-bounds index (both revert).
    for i in 0..(long_a.len() + 1) {
        let data = encode_abi("at(string,uint256)", &[Arg::Dyn(long_a), Arg::Static(word_u256(U256::from(i)))]);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert_eq!(g.success, s.success, "at({}) success differs (gum={}, sol={})", i, g.success, s.success);
        if g.success {
            assert_eq!(g.output, s.output, "at({}) output differs", i);
        }
    }

    // Slice: valid ranges, plus inverted and past-the-end (both must revert).
    let slices: &[(u64, u64)] = &[(0, 0), (0, 5), (5, 5), (3, 40), (0, 61), (10, 61), (5, 3), (0, 62), (61, 62)];
    for (s0, e0) in slices {
        let data = encode_abi(
            "cut(string,uint256,uint256)",
            &[Arg::Dyn(long_a), Arg::Static(word_u256(U256::from(*s0))), Arg::Static(word_u256(U256::from(*e0)))],
        );
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert_eq!(g.success, s.success, "cut({},{}) success differs (gum={}, sol={})", s0, e0, g.success, s.success);
        if g.success {
            assert_eq!(g.output, s.output, "cut({},{}) output differs", s0, e0);
        }
    }
}

#[test]
fn assert_message_forms_match_solidity() {
    // assert(cond, "text")      -> Error(string), identical to require(c, "text")
    // assert(cond, MyErr(a))    -> that custom error's encoding
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "\
use gum.defaults.String

enum Errors:
    TooSmall(u256 got)

contract App:
    export fn need_big(u256 x) -> u256:
        assert(x > 10, \"value too small\")
        return x

    export fn need_big_err(u256 x) -> u256:
        assert(x > 10, Errors.TooSmall(x))
        return x
";
    let sol_src = "\
// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
error TooSmall(uint256 got);
contract A {
    function need_big(uint256 x) external pure returns (uint256) {
        require(x > 10, \"value too small\");
        return x;
    }
    function need_big_err(uint256 x) external pure returns (uint256) {
        if (!(x > 10)) revert TooSmall(x);
        return x;
    }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    for sig in ["need_big(uint256)", "need_big_err(uint256)"] {
        for x in [5u64, 11u64] {
            let data = encode(sig, &[U256::from(x)]);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(g.success, s.success, "{} x={} success differs", sig, x);
            assert_eq!(g.output, s.output, "{} x={} revert/return data differs", sig, x);
        }
    }
}

#[test]
fn dynamic_constructor_args_match_solidity() {
    // A constructor taking a dynamic String alongside a static u256. The
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "\
use gum.defaults.String

contract Meta:
    u256 name_len
    u256 id

    fn new(String name, u256 id):
        Meta.name_len = name.length
        Meta.id = id

    export fn len_of() -> u256:
        return Meta.name_len

    export fn id_of() -> u256:
        return Meta.id
";
    let sol_src = "\
// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Meta {
    uint256 name_len;
    uint256 id;
    constructor(string memory name_, uint256 id_) {
        name_len = bytes(name_).length;
        id = id_;
    }
    function len_of() external view returns (uint256) { return name_len; }
    function id_of() external view returns (uint256) { return id; }
}
";

    for name in [b"".as_ref(), b"gum".as_ref(), b"a name longer than thirty-two bytes for padding".as_ref()] {
        // ABI-encode (string, uint256) with no selector, that's exactly what
        // gets appended to the creation code.
        let args = &encode_abi("x(string,uint256)", &[Arg::Dyn(name), Arg::Static(word_u256(U256::from(7u64)))])[4..];

        let mut gum_code = gum_creation_bytecode(gum_src, &solc, false);
        gum_code.extend_from_slice(args);
        let mut sol_code = sol_creation_bytecode(sol_src, &solc);
        sol_code.extend_from_slice(args);

        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let gaddr = deploy(&mut gdb, gum_code);
        let saddr = deploy(&mut sdb, sol_code);

        assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(name.len()), "gum name_len for {:?}", name);
        assert_eq!(storage(&mut gdb, gaddr, 1), U256::from(7u64), "gum id");

        for sig in ["len_of()", "id_of()"] {
            let g = call(&mut gdb, gaddr, selector(sig).to_vec());
            let s = call(&mut sdb, saddr, selector(sig).to_vec());
            assert!(g.success && s.success, "{} failed for {:?}", sig, name);
            assert_eq!(g.output, s.output, "{} differs from Solidity for {:?}", sig, name);
        }
    }
}

const GUM_PAYABLE: &str = include_str!("fixtures/gum_payable.gum");

const SOL_PAYABLE: &str = include_str!("fixtures/sol_payable.sol");

#[test]
fn payable_accepts_eth_and_nonpayable_still_rejects_it() {
    // The regression this guards: the ETH-rejection guard used to be hoisted to
    // the dispatcher entry and skipped entirely the moment any function was
    // payable, which would have let poke() and total_of() silently accept
    // and trap ETH. Each non-payable case must carry its own guard.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_PAYABLE, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_PAYABLE, &solc));

    let wei = U256::from(1_000u64);

    // payable: value-bearing call succeeds and msg.value lands in storage.
    let d = selector("deposit()").to_vec();
    let g = call_with_value(&mut gdb, deployer(), gaddr, d.clone(), wei);
    let s = call_with_value(&mut sdb, deployer(), saddr, d, wei);
    assert!(g.success && s.success, "payable deposit must accept ETH (gum={}, sol={})", g.success, s.success);
    assert_eq!(storage(&mut gdb, gaddr, 0), wei, "gum: msg.value must reach storage");
    assert_eq!(storage(&mut gdb, gaddr, 0), storage(&mut sdb, saddr, 0), "deposit storage must match Solidity");

    // non-payable, in a contract that HAS a payable function: must still reject.
    for sig in ["poke()", "total_of()"] {
        let g = call_with_value(&mut gdb, deployer(), gaddr, selector(sig).to_vec(), wei);
        let s = call_with_value(&mut sdb, deployer(), saddr, selector(sig).to_vec(), wei);
        assert!(!s.success, "sanity: Solidity {} must reject ETH", sig);
        assert!(!g.success, "{} is not payable and must reject ETH, but it succeeded", sig);
        assert_eq!(g.success, s.success, "{} value-call behavior must match Solidity", sig);
    }

    // ...and the same calls still work with no value attached.
    for sig in ["poke()", "total_of()"] {
        let g = call(&mut gdb, gaddr, selector(sig).to_vec());
        let s = call(&mut sdb, saddr, selector(sig).to_vec());
        assert!(g.success && s.success, "{} must succeed with no value", sig);
        assert_eq!(g.output, s.output, "{} output must match Solidity", sig);
    }
}

#[test]
fn bare_return_early_exits_void_function() {
    // return with no value in a function that declares no return type must
    // actually skip the rest of the body, matching Solidity's bare return;.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "
contract S:
    u256 t

    export fn set_unless_zero(u256 x):
        if x == 0:
            return
        S.t = x

    export fn t_of() -> u256:
        return S.t
";
    let sol_src = "\
// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract S {
    uint256 t;
    function set_unless_zero(uint256 x) external {
        if (x == 0) { return; }
        t = x;
    }
    function t_of() external view returns (uint256) { return t; }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    for x in [7u64, 0u64] {
        let data = encode("set_unless_zero(uint256)", &[U256::from(x)]);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert!(g.success && s.success, "set_unless_zero({}) failed", x);
        assert_eq!(g.output, s.output, "set_unless_zero({}) output differs", x);
        assert_eq!(
            storage(&mut gdb, gaddr, 0),
            storage(&mut sdb, saddr, 0),
            "storage after set_unless_zero({}) differs from Solidity", x
        );
    }
    // x=0 took the early return, so the earlier x=7 write must still stand.
    assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(7u64), "early return must skip the write");
}

// A vault whose entry point hands control to an untrusted address mid-body ,
// the classic reentrancy shape. {MOD} is swapped for unsafe  to opt the
// guard back out.
const GUM_REENTRANT: &str = include_str!("fixtures/gum_reentrant.gum");

// Calls poke() again from inside ping(), i.e. re-enters while the first poke is
// still in flight. It does NOT swallow the failure, so a blocked re-entry
// propagates out and fails the whole transaction.
// Same shape as GUM_REENTRANT, but poke() returns a value, a different
// codegen path for the lock release, and the one exercised below.
const GUM_GUARDED_RETURNING: &str = include_str!("fixtures/gum_guarded_returning.gum");

// Calls the target twice, sequentially, never nested. Both calls must succeed:
// this is ordinary batching (a router, a multicall), not reentrancy.
const SOL_BATCHER: &str = include_str!("fixtures/sol_batcher.sol");

#[test]
fn guard_releases_the_lock_on_a_value_returning_entry_point() {
    // The reentrancy lock must be released when a guarded entry point
    // returns, not merely set. Transient storage clears at the end of the
    // transaction, not the call frame, so a lock left set stays set for the
    // rest of the tx, and the next perfectly legitimate call into the contract
    // reverts. Two sequential (non-nested) calls in one transaction is the
    // cheapest thing that can tell "released" apart from "leaked".
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let vault = deploy(&mut db, gum_creation_bytecode(GUM_GUARDED_RETURNING, &solc, false));
    let batcher = deploy(&mut db, sol_creation_bytecode(SOL_BATCHER, &solc));
    assert!(call(&mut db, batcher, encode_words("setTarget(address)", &[word_addr(vault)])).success);

    let r = call(&mut db, batcher, selector("twice()").to_vec());
    assert!(
        r.success,
        "two sequential calls in one transaction must both succeed, the guard \
         must release its lock on return, not leak it for the rest of the tx"
    );
    assert_eq!(storage(&mut db, vault, 0), U256::from(2u64), "both calls should have bumped the counter");
}

// Field widths are all 32 here (a dynamic array's length slot, a slot-aligned
// fixed array, a word), so gum's size-ordered packer keeps declaration order
// and the two contracts agree slot-for-slot: a->0, f->1, sentinel->2.
// An array whose element is a struct wider than a word. Solidity gives each
// such element whole slots, never packing two into one, so stakes[i] starts
// at base + i2 and its two fields sit at +0 and +1.
const GUM_STRUCT_ARR: &str = include_str!("fixtures/gum_struct_arr.gum");

const SOL_STRUCT_ARR: &str = r#"
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;
contract V {
    struct Stake { uint256 amount; uint256 since; }
    uint256 total;
    Stake[] stakes;

    function add(uint256 a, uint256 s) external {
        stakes.push();
        stakes[stakes.length - 1].amount = a;
        stakes[stakes.length - 1].since = s;
        total = total + a;
    }
    function set_amount(uint256 i, uint256 a) external { stakes[i].amount = a; }
    function get_amount(uint256 i) external view returns (uint256) { return stakes[i].amount; }
    function get_since(uint256 i) external view returns (uint256) { return stakes[i].since; }
    function len() external view returns (uint256) { return stakes.length; }
    function drop() external { stakes.pop(); }
}
"#;

#[test]
fn struct_array_layout_matches_solidity() {
    // A struct element occupies whole slots and its fields sit at fixed offsets
    // from the element's base, the same rule a struct in a mapping follows,
    // just addressed by index instead of by hash.
    //
    // Diff the raw slots, not only the getters. This path used to store the
    // pushed struct's memory pointer into a single slot and mload it back on
    // read, which returned zero for every field once memory was fresh; a
    // getters-only test that compared gum against gum would have looked fine.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    // --rich-reverts so the out-of-bounds and empty-pop panics compare
    // byte-for-byte rather than merely both-failing.
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STRUCT_ARR, &solc, true));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STRUCT_ARR, &solc));

    macro_rules! same_storage {
        ($ctx:expr) => {{
            for slot in 0..2u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot), storage(&mut sdb, saddr, slot),
                    "field slot {} differs after {}", slot, $ctx
                );
            }
            // Six data slots: three elements' worth, enough to catch a stride
            // of 1 (packing two structs into the space of one) as well as any
            // spill past where a correct layout ends.
            let base = dyn_array_data_base(1);
            for i in 0..6u64 {
                let s = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, gaddr, s), storage_at(&mut sdb, saddr, s),
                    "struct array data slot {} differs after {}", i, $ctx
                );
            }
        }};
    }
    macro_rules! both {
        ($sig:expr, $words:expr) => {{
            let data = encode_words($sig, $words);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(g.success, s.success, "success differs for {}", $sig);
            assert_eq!(g.output, s.output, "output differs for {}", $sig);
            g
        }};
    }

    // Three elements, each with two distinct fields, so a stride error or a
    // field-offset error shows up as a slot mismatch rather than by luck.
    for (a, s) in [(7u64, 9u64), (11, 13), (17, 19)] {
        both!("add(uint256,uint256)", &[word_u256(U256::from(a)), word_u256(U256::from(s))]);
    }
    same_storage!("three pushes");

    let n = both!("len()", &[]);
    assert_eq!(U256::from_be_slice(&n.output), U256::from(3u64), "length should be 3");

    // Read every field back: the values must survive, which is precisely what
    // the old pointer-storing path failed to do.
    for (i, (a, s)) in [(0u64, (7u64, 9u64)), (1, (11, 13)), (2, (17, 19))] {
        let ga = both!("get_amount(uint256)", &[word_u256(U256::from(i))]);
        assert_eq!(U256::from_be_slice(&ga.output), U256::from(a), "stakes[{}].amount", i);
        let gs = both!("get_since(uint256)", &[word_u256(U256::from(i))]);
        assert_eq!(U256::from_be_slice(&gs.output), U256::from(s), "stakes[{}].since", i);
    }

    // Writing one element's field must not disturb its neighbours.
    both!("set_amount(uint256,uint256)", &[word_u256(U256::from(1u64)), word_u256(U256::from(99u64))]);
    same_storage!("overwriting stakes[1].amount");
    let gs = both!("get_since(uint256)", &[word_u256(U256::from(1u64))]);
    assert_eq!(U256::from_be_slice(&gs.output), U256::from(13u64), "neighbour field was clobbered");

    // Out of bounds panics identically.
    let oob = both!("get_amount(uint256)", &[word_u256(U256::from(3u64))]);
    assert!(!oob.success, "index 3 of a 3-element array must revert");

    // pop must zero both of the removed element's slots, like Solidity.
    both!("drop()", &[]);
    same_storage!("pop");
    let n = both!("len()", &[]);
    assert_eq!(U256::from_be_slice(&n.output), U256::from(2u64), "length should be 2 after pop");

    // Popping down to empty, then one more: the empty-pop panic must match.
    both!("drop()", &[]);
    both!("drop()", &[]);
    same_storage!("popping to empty");
    let empty = both!("drop()", &[]);
    assert!(!empty.success, "popping an empty array must revert");
}

const GUM_PACKED_ARR: &str = include_str!("fixtures/gum_packed_arr.gum");

const SOL_PACKED_ARR: &str = include_str!("fixtures/sol_packed_arr.sol");

#[test]
fn packed_storage_array_layout_matches_solidity() {
    // Elements narrower than a word share a slot (32 uint8s per slot) for
    // both dynamic and fixed arrays. Diff the raw slots, not just the getters:
    // giving each element its own slot reads back perfectly consistently, and
    // is still wrong.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    // --rich-reverts, so gum emits the same Panic(uint256) data Solidity does
    // and the out-of-bounds / empty-pop cases can be compared byte-for-byte
    // rather than merely both-failed.
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_PACKED_ARR, &solc, true));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_PACKED_ARR, &solc));

    // Compares every slot either contract could legitimately touch: the three
    // field slots, and enough of the dynamic array's data region to catch an
    // unpacked layout spilling past where the packed one ends.
    macro_rules! same_storage {
        ($ctx:expr) => {{
            for slot in 0..3u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot), storage(&mut sdb, saddr, slot),
                    "field slot {} differs after {}", slot, $ctx
                );
            }
            let base = dyn_array_data_base(0);
            for i in 0..4u64 {
                let s = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, gaddr, s), storage_at(&mut sdb, saddr, s),
                    "dynamic array data slot {} differs after {}", i, $ctx
                );
            }
        }};
    }

    // Same call to both, same result required.
    macro_rules! both {
        ($sig:expr, $words:expr) => {{
            let data = encode_words($sig, $words);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(g.success, s.success, "success differs for {}", $sig);
            assert_eq!(g.output, s.output, "output differs for {}", $sig);
            g
        }};
    }

    // The fixed array packs all four elements into slot 1.
    for (i, v) in [(0u64, 0xaau64), (1, 0xbb), (2, 0xcc), (3, 0xdd)] {
        both!("setf(uint256,uint8)", &[word_u256(U256::from(i)), word_u256(U256::from(v))]);
        both!("getf(uint256)", &[word_u256(U256::from(i))]);
    }
    same_storage!("filling the fixed array");
    assert_ne!(storage(&mut gdb, gaddr, 1), U256::ZERO, "the fixed array should be in slot 1");
    assert_eq!(storage(&mut gdb, gaddr, 2), U256::ZERO, "the fixed array must not have spilled onto sentinel");

    // Push past a slot boundary: 33 elements need exactly two slots.
    for v in 0..33u64 {
        both!("push(uint8)", &[word_u256(U256::from(v + 1))]);
    }
    same_storage!("33 pushes");
    both!("len()", &[]);
    both!("sum()", &[]);
    for i in 0..33u64 {
        both!("get(uint256)", &[word_u256(U256::from(i))]);
    }

    // Out of bounds must revert on both.
    let r = both!("get(uint256)", &[word_u256(U256::from(33u64))]);
    assert!(!r.success, "index 33 of a 33-element array must revert");

    // Popping back across the boundary must zero the vacated element without
    // disturbing the neighbours sharing its slot.
    for _ in 0..3 {
        both!("pop()", &[]);
    }
    same_storage!("popping back across the slot boundary");
    both!("sum()", &[]);

    // Drain it, then confirm an empty pop reverts on both.
    for _ in 0..30 {
        both!("pop()", &[]);
    }
    same_storage!("draining the array");
    let r = both!("pop()", &[]);
    assert!(!r.success, "popping an empty array must revert");
}

// Every kind of storage delete has to reach, in one contract. gum's packer
// sorts by width (ties keep declaration order), so the 32-byte fields take
// slots 0..5 in declaration order and the two u8s share slot 6, which is
// exactly the order the Solidity twin declares them in, so the slots line up.
const GUM_DELETE: &str = include_str!("fixtures/gum_delete.gum");

const SOL_DELETE: &str = include_str!("fixtures/sol_delete.sol");

#[test]
fn delete_matches_solidity() {
    // delete has to reach every shape of storage: scalars, a packed field
    // (without disturbing its slot-mate), a long storage string's data slots, a
    // dynamic array's elements and length, a fixed array, a mapping entry, and
    // a struct behind a mapping. Diff raw slots against Solidity's own
    // delete, since the whole point is releasing storage, a delete that only
    // reads back as zero while leaving words behind would pass a getter test
    // and still be wrong.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_DELETE, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_DELETE, &solc));

    let who = Address::from([0x55u8; 20]);

    // Every slot either contract can touch: the eight field slots, the string's
    // and array's data regions, and the mapping entries for who.
    let mut watched: Vec<U256> = (0..8u64).map(U256::from).collect();
    for i in 0..4u64 {
        watched.push(dyn_array_data_base(1) + U256::from(i)); // name's long form
        watched.push(dyn_array_data_base(2) + U256::from(i)); // arr's elements
    }
    watched.push(mapping_slot(who, 4)); // bal[who]
    watched.push(mapping_slot(who, 5)); // stakes[who].amount
    watched.push(mapping_slot(who, 5) + U256::from(1u64)); // stakes[who].since

    macro_rules! same_storage {
        ($ctx:expr) => {
            for slot in &watched {
                assert_eq!(
                    storage_at(&mut gdb, gaddr, *slot),
                    storage_at(&mut sdb, saddr, *slot),
                    "slot {} differs after {}", slot, $ctx
                );
            }
        };
    }
    macro_rules! both {
        ($sig:expr, $data:expr) => {{
            let g = call(&mut gdb, gaddr, $data.clone());
            let s = call(&mut sdb, saddr, $data);
            assert!(g.success && s.success, "{} failed", $sig);
            assert_eq!(g.output, s.output, "output differs for {}", $sig);
        }};
    }

    // A name past 31 bytes, so it takes the long form and owns real data slots
    // that delete has to release.
    let long_name = vec![b'q'; 70];
    let fill = encode_abi("fill(address,string)", &[Arg::Static(word_addr(who)), Arg::Dyn(&long_name)]);
    both!("fill", fill);
    same_storage!("fill");
    // Guard against the whole test passing vacuously on empty storage.
    assert_ne!(storage(&mut gdb, gaddr, 0), U256::ZERO, "fill should have written something");
    assert_ne!(
        storage_at(&mut gdb, gaddr, dyn_array_data_base(1)),
        U256::ZERO,
        "the long name should occupy data slots"
    );

    both!("wipe", encode_words("wipe(address)", &[word_addr(who)]));
    same_storage!("wipe");
    both!("len()", selector("len()").to_vec());
    both!("packed_b()", selector("packed_b()").to_vec());

    // Everything deleted must actually be gone from storage, not merely read
    // back as zero.
    for slot in &watched {
        let v = storage_at(&mut gdb, gaddr, *slot);
        if *slot == U256::from(6u64) {
            // packed_b shares this slot and was NOT deleted, it must survive.
            assert_eq!(v, U256::from(9u64) << 8, "deleting packed_a must not disturb packed_b");
        } else {
            assert_eq!(v, U256::ZERO, "slot {} should be zeroed after delete", slot);
        }
    }
}

// A Vec(T) contract field is a storage vector: same layout as [T], and both
// spellings of each operation (get/[i], len()/.length) reach it.
const GUM_STORAGE_VEC: &str = include_str!("fixtures/gum_storage_vec.gum");

const SOL_STORAGE_VEC: &str = include_str!("fixtures/sol_storage_vec.sol");

#[test]
fn storage_vec_matches_a_solidity_dynamic_array() {
    // Vec(T) in a contract used to be rejected outright. It now compiles to the
    // dynamic-array storage layout, so the test is whether it is slot-for-slot
    // indistinguishable from Solidity's uint256[], including that a storage
    // push mutates in place, with none of the memory Vec's v = v.push(x).
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STORAGE_VEC, &solc, true));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STORAGE_VEC, &solc));

    macro_rules! both {
        ($sig:expr, $words:expr) => {{
            let data = encode_words($sig, $words);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(g.success, s.success, "success differs for {}", $sig);
            assert_eq!(g.output, s.output, "output differs for {}", $sig);
            g
        }};
    }
    macro_rules! same_storage {
        ($ctx:expr) => {{
            for slot in 0..2u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot), storage(&mut sdb, saddr, slot),
                    "field slot {} differs after {}", slot, $ctx
                );
            }
            let base = dyn_array_data_base(0);
            for i in 0..6u64 {
                let s = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, gaddr, s), storage_at(&mut sdb, saddr, s),
                    "element slot {} differs after {}", i, $ctx
                );
            }
        }};
    }

    both!("set_sentinel(uint256)", &[word_u256(U256::from(0xfeedu64))]);
    for v in [11u64, 22, 33, 44, 55] {
        both!("add(uint256)", &[word_u256(U256::from(v))]);
    }
    same_storage!("pushes");
    both!("len()", &[]);
    both!("length()", &[]);
    for i in 0..5u64 {
        both!("get(uint256)", &[word_u256(U256::from(i))]);
        both!("at(uint256)", &[word_u256(U256::from(i))]);
    }
    // Both spellings must agree with each other, not just with Solidity.
    assert_eq!(
        call(&mut gdb, gaddr, encode_words("get(uint256)", &[word_u256(U256::from(2u64))])).output,
        call(&mut gdb, gaddr, encode_words("at(uint256)", &[word_u256(U256::from(2u64))])).output,
        "v.get(i) and v[i] must be the same element"
    );

    let r = both!("get(uint256)", &[word_u256(U256::from(5u64))]);
    assert!(!r.success, "reading past the end must revert");

    both!("drop()", &[]);
    both!("drop()", &[]);
    same_storage!("pops");
    both!("len()", &[]);

    both!("wipe()", &[]);
    same_storage!("delete");
    both!("len()", &[]);
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        U256::from(0xfeedu64),
        "wiping the vector must leave the neighbouring field alone"
    );
}

// A two-level base chain, the shape inheritance actually gets used for: a base
// holding the ledger, a middle layer adding ownership, a contract adding its
// own field and overriding a method.
//
// credit and claim are exported on the bases. Neither base is a
// contract, so they are not entry points there, they become entry points on
// Bank by being inherited, which is the whole point: a base class carries a
// reusable slice of a contract's public surface (Ownable, ERC20) instead of
// being copy-pasted into it.
const GUM_INHERIT: &str = include_str!("fixtures/gum_inherit.gum");

const SOL_INHERIT: &str = include_str!("fixtures/sol_inherit.sol");

#[test]
fn inheritance_matches_solidity() {
    // Solidity lays inherited state out most-base-first, and so does gum, so a
    // three-level chain must agree slot-for-slot: total->0, owner->1, fee->2.
    // The override must win at every level, and an inherited method must bind
    // to the child's storage.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_INHERIT, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_INHERIT, &solc));

    macro_rules! both {
        ($sig:expr, $words:expr) => {{
            let data = encode_words($sig, $words);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(g.success, s.success, "success differs for {}", $sig);
            assert_eq!(g.output, s.output, "output differs for {}", $sig);
        }};
    }

    // An inherited method writing inherited state.
    both!("credit(uint256)", &[word_u256(U256::from(30u64))]);
    both!("credit(uint256)", &[word_u256(U256::from(12u64))]);
    // An inherited method from the middle of the chain.
    both!("claim()", &[]);
    // The contract's own field.
    both!("set_fee(uint256)", &[word_u256(U256::from(9u64))]);

    both!("total()", &[]);
    both!("owner()", &[]);
    both!("fee()", &[]);
    // The override, not Ledger's 100.
    both!("cap_of()", &[]);

    for slot in 0..3u64 {
        assert_eq!(
            storage(&mut gdb, gaddr, slot),
            storage(&mut sdb, saddr, slot),
            "inherited storage slot {} differs", slot
        );
    }
    // Nail the layout down rather than only proving the two agree.
    assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(42u64), "Ledger.total belongs in slot 0");
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        U256::from_be_slice(deployer().as_slice()),
        "Owned.owner belongs in slot 1"
    );
    assert_eq!(storage(&mut gdb, gaddr, 2), U256::from(9u64), "Bank.fee belongs in slot 2");
}

const GUM_BUBBLE: &str = include_str!("fixtures/gum_bubble.gum");

// A token that reverts the way real ones do: a require string, and a custom
// error. Both must reach the original caller intact.
const SOL_REVERTING_TOKEN: &str = include_str!("fixtures/sol_reverting_token.sol");

// The same call made directly from Solidity, to compare against.
const SOL_BUBBLE_CALLER: &str = include_str!("fixtures/sol_bubble_caller.sol");

#[test]
fn external_call_reverts_bubble_up_like_solidity() {
    // A failing sub-call carries the only diagnostic anyone gets: an ERC20's
    // require string, a custom error's ABI encoding. Reverting blank throws it
    // away. The bar is Solidity's own behaviour, so diff against a Solidity
    // caller making the identical call rather than hand-asserting the bytes.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let gum_caller = deploy(&mut db, gum_creation_bytecode(GUM_BUBBLE, &solc, false));
    let sol_caller = deploy(&mut db, sol_creation_bytecode(SOL_BUBBLE_CALLER, &solc));
    let token = deploy(&mut db, sol_creation_bytecode(SOL_REVERTING_TOKEN, &solc));
    let to = Address::from([0x99u8; 20]);

    for (amount, what) in [(1u64, "an Error(string) require"), (2, "a custom error")] {
        let data = encode_words(
            "send(address,address,uint256)",
            &[word_addr(token), word_addr(to), word_u256(U256::from(amount))],
        );
        let g = call(&mut db, gum_caller, data.clone());
        let s = call(&mut db, sol_caller, data);
        assert!(!g.success && !s.success, "both callers should revert for {}", what);
        assert!(!g.output.is_empty(), "gum swallowed the revert data for {}", what);
        assert_eq!(g.output, s.output, "gum's bubbled revert data must match Solidity's for {}", what);
    }

    // The reason really is the token's own, not something gum invented.
    let data = encode_words(
        "send(address,address,uint256)",
        &[word_addr(token), word_addr(to), word_u256(U256::from(1u64))],
    );
    let out = call(&mut db, gum_caller, data).output;
    assert_eq!(&out[..4], &[0x08, 0xc3, 0x79, 0xa0], "expected an Error(string) selector");
    assert!(
        String::from_utf8_lossy(&out).contains("ERC20: transfer amount exceeds balance"),
        "the token's own reason string must survive the call boundary, got {:?}",
        out
    );

    // A successful call must still work.
    let data = encode_words(
        "send(address,address,uint256)",
        &[word_addr(token), word_addr(to), word_u256(U256::from(5u64))],
    );
    let g = call(&mut db, gum_caller, data);
    assert!(g.success, "a succeeding transfer must still succeed");
    assert_eq!(g.output, word_u256(U256::from(1u64)).to_vec(), "should return true");
}

const GUM_RECEIVE: &str = include_str!("fixtures/gum_receive.gum");

const SOL_RECEIVE: &str = include_str!("fixtures/sol_receive.sol");

// No receive: a plain ETH send has nowhere to go and must revert.
const GUM_NO_RECEIVE: &str = include_str!("fixtures/gum_no_receive.gum");

#[test]
fn receive_and_fallback_match_solidity() {
    // A contract with no receive rejects plain ETH, that is the blocker. With
    // one, a bare send must land, and the dispatch rules (empty calldata ->
    // receive, unmatched selector -> fallback, stray 1-3 bytes -> fallback)
    // must match Solidity's exactly.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_RECEIVE, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_RECEIVE, &solc));

    // Same call + value to both; identical outcome required.
    macro_rules! both_val {
        ($data:expr, $wei:expr, $what:expr) => {{
            let g = call_with_value(&mut gdb, deployer(), gaddr, $data, U256::from($wei as u64));
            let s = call_with_value(&mut sdb, deployer(), saddr, $data, U256::from($wei as u64));
            assert_eq!(g.success, s.success, "success differs for {}", $what);
            assert_eq!(g.output, s.output, "output differs for {}", $what);
            for slot in 0..2u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot), storage(&mut sdb, saddr, slot),
                    "slot {} differs after {}", slot, $what
                );
            }
            g
        }};
    }

    // A plain ETH send: empty calldata -> receive.
    let r = both_val!(vec![], 1_000, "a bare ETH send");
    assert!(r.success, "a plain ETH send must reach receive()");
    assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(1_000u64), "receive should have banked the ETH");
    assert_eq!(gdb.basic(gaddr).unwrap().unwrap().balance, U256::from(1_000u64));

    both_val!(vec![], 500, "a second bare ETH send");
    assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(1_500u64));

    // Empty calldata with no value still routes to receive.
    both_val!(vec![], 0, "an empty call with no value");

    // An unmatched selector -> fallback, not receive.
    both_val!(vec![0xde, 0xad, 0xbe, 0xef], 0, "an unmatched selector");
    assert_eq!(storage(&mut gdb, gaddr, 1), U256::from(1u64), "fallback should have run");
    assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(1_500u64), "receive must not have run");

    // 1-3 stray bytes: too few to hold a selector -> fallback.
    both_val!(vec![0x01, 0x02], 0, "two stray calldata bytes");
    assert_eq!(storage(&mut gdb, gaddr, 1), U256::from(2u64), "short calldata should reach fallback");

    // A real selector still dispatches normally.
    let r = both_val!(selector("total()").to_vec(), 0, "total()");
    assert!(r.success);
    assert_eq!(r.output, word_u256(U256::from(1_500u64)).to_vec());

    // Without a receive, a plain send must revert rather than silently trap ETH.
    let mut ndb: Db = CacheDB::new(EmptyDB::default());
    let naddr = deploy(&mut ndb, gum_creation_bytecode(GUM_NO_RECEIVE, &solc, false));
    let r = call_with_value(&mut ndb, deployer(), naddr, vec![], U256::from(10u64));
    assert!(!r.success, "a contract with no receive must reject a plain ETH send");
    assert_eq!(ndb.basic(naddr).unwrap().map(|a| a.balance).unwrap_or_default(), U256::ZERO);
}

#[test]
fn a_payable_receive_does_not_disarm_the_guard_on_other_functions() {
    // The hoisted nonpayable guard is only sound when nothing is payable. A
    // payable receive makes something payable, so the hoist must be
    // suppressed and every other entry point must carry its own guard ,
    // otherwise poke() would silently accept and trap ETH.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = "\
use gum.defaults.Message

contract V:
    u256 got

    export payable fn receive():
        V.got = V.got + Message.value()

    export fn poke():
        V.got = 0
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    assert!(
        call_with_value(&mut db, deployer(), addr, vec![], U256::from(5u64)).success,
        "receive must accept ETH"
    );
    let r = call_with_value(&mut db, deployer(), addr, selector("poke()").to_vec(), U256::from(5u64));
    assert!(!r.success, "poke() is not payable and must still reject ETH even though receive() is payable");
}

const GUM_FACTORY: &str = include_str!("fixtures/gum_factory.gum");

const SOL_CHILD: &str = include_str!("fixtures/sol_child.sol");

const SOL_BAD_CHILD: &str = include_str!("fixtures/sol_bad_child.sol");

// EIP-1014: keccak256(0xff ++ deployer ++ salt ++ keccak256(code))[12:].
// Computed here independently rather than asking a second Solidity contract,
// so the test checks gum against the spec and not against another guess.
fn create2_address(deployer: Address, salt: U256, code: &[u8]) -> Address {
    use tiny_keccak::{Hasher, Keccak};
    let mut code_hash = [0u8; 32];
    let mut k = Keccak::v256();
    k.update(code);
    k.finalize(&mut code_hash);

    let mut buf = Vec::with_capacity(85);
    buf.push(0xff);
    buf.extend_from_slice(deployer.as_slice());
    buf.extend_from_slice(&salt.to_be_bytes::<32>());
    buf.extend_from_slice(&code_hash);

    let mut out = [0u8; 32];
    let mut k = Keccak::v256();
    k.update(&buf);
    k.finalize(&mut out);
    Address::from_slice(&out[12..])
}

fn addr_from_word(out: &[u8]) -> Address {
    Address::from_slice(&out[12..32])
}

#[test]
fn factory_create_and_create2_deploy_real_contracts() {
    // A gum contract deploying another contract: CREATE from creation bytecode,
    // CREATE2 at an address knowable in advance, and a failing child
    // constructor surfacing its own reason.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let factory = deploy(&mut db, gum_creation_bytecode(GUM_FACTORY, &solc, false));
    let child_code = sol_creation_bytecode(SOL_CHILD, &solc);

    // --- CREATE ---
    let r = call(&mut db, factory, encode_abi("deploy(bytes)", &[Arg::Dyn(&child_code)]));
    assert!(r.success, "deploy failed: {:?}", r.output);
    let child = addr_from_word(&r.output);
    assert_ne!(child, Address::ZERO, "should have returned a real address");

    let info = db.basic(child).unwrap().expect("child account should exist");
    assert!(info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false), "child should have runtime code");

    // The child is real: its constructor ran, and it answers calls.
    let g = call(&mut db, child, selector("get()").to_vec());
    assert!(g.success, "child call failed");
    assert_eq!(g.output, word_u256(U256::from(42u64)).to_vec(), "the child's constructor should have run");
    assert_eq!(storage(&mut db, factory, 0), U256::from(1u64), "the factory should have counted the deploy");

    // --- CREATE2 ---
    let salt = U256::from(0xcafeu64);
    let expected = create2_address(factory, salt, &child_code);

    // predict() must agree before anything is deployed there.
    let p = call(
        &mut db,
        factory,
        encode_abi("predict(bytes,uint256)", &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt))]),
    );
    assert!(p.success, "predict failed");
    assert_eq!(addr_from_word(&p.output), expected, "create2_address must match EIP-1014");
    assert!(
        db.basic(expected).unwrap().map(|a| a.code.map(|c| c.is_empty()).unwrap_or(true)).unwrap_or(true),
        "nothing should be deployed at the predicted address yet"
    );

    let r = call(
        &mut db,
        factory,
        encode_abi("deploy2(bytes,uint256)", &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt))]),
    );
    assert!(r.success, "deploy2 failed: {:?}", r.output);
    assert_eq!(addr_from_word(&r.output), expected, "create2 must land on the predicted address");
    let g = call(&mut db, expected, selector("get()").to_vec());
    assert!(g.success && g.output == word_u256(U256::from(42u64)).to_vec(), "the create2'd child should work");

    // Same salt twice: the address is taken, so CREATE2 returns 0 and gum must
    let r = call(
        &mut db,
        factory,
        encode_abi("deploy2(bytes,uint256)", &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt))]),
    );
    assert!(!r.success, "redeploying at the same create2 address must revert, not return address 0");

    // A different salt is a different address.
    let salt2 = U256::from(0xbeefu64);
    let r = call(
        &mut db,
        factory,
        encode_abi("deploy2(bytes,uint256)", &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt2))]),
    );
    assert!(r.success, "a fresh salt should deploy");
    assert_eq!(addr_from_word(&r.output), create2_address(factory, salt2, &child_code));

    // --- a failing child constructor ---
    let bad = sol_creation_bytecode(SOL_BAD_CHILD, &solc);
    let r = call(&mut db, factory, encode_abi("deploy(bytes)", &[Arg::Dyn(&bad)]));
    assert!(!r.success, "a reverting child constructor must fail the deploy");
    assert!(
        String::from_utf8_lossy(&r.output).contains("child ctor failed"),
        "the child constructor's own revert reason must bubble up, got {:?}",
        r.output
    );
}

#[test]
fn factory_can_fund_the_contract_it_deploys() {
    // CREATE forwards value to the child's constructor.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let factory = deploy(&mut db, gum_creation_bytecode(GUM_FACTORY, &solc, false));
    let child_code = sol_creation_bytecode(SOL_CHILD, &solc);

    let wei = U256::from(777u64);
    let r = call_with_value(
        &mut db,
        deployer(),
        factory,
        encode_abi("deploy_funded(bytes)", &[Arg::Dyn(&child_code)]),
        wei,
    );
    assert!(r.success, "funded deploy failed: {:?}", r.output);
    let child = addr_from_word(&r.output);
    assert_eq!(db.basic(child).unwrap().unwrap().balance, wei, "the ETH should have gone to the child");
    assert_eq!(
        db.basic(factory).unwrap().unwrap().balance,
        U256::ZERO,
        "the factory should not have kept it"
    );
}

// new Child(...) from inside a contract. Child's creation code is embedded in
// Factory as a Yul sub-object, so Factory can deploy it with no help from the
// caller, the Uniswap-V2 new Pair() shape.
const GUM_NEW_CONTRACT: &str = include_str!("fixtures/gum_new_contract.gum");

// Same thing in Solidity, to diff against.
const SOL_NEW_CONTRACT: &str = include_str!("fixtures/sol_new_contract.sol");

fn deploy_named(db: &mut Db, solc: &Path, src: &str, name: &str) -> Address {
    deploy(db, gum_creation_bytecode_for(src, solc, false, name))
}

#[test]
fn new_contract_deploys_a_real_child() {
    // The child's creation code has to travel inside the factory's runtime,
    // its constructor has to run with the factory as msg.sender, and the
    // returned address has to be a live contract. Diffed against Solidity's
    // new Child(x), which is the same operation.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gfac = deploy_named(&mut gdb, &solc, GUM_NEW_CONTRACT, "Factory");
    let sfac = deploy(&mut sdb, sol_creation_bytecode_for(SOL_NEW_CONTRACT, &solc, "Factory"));

    macro_rules! both {
        ($sig:expr, $words:expr) => {{
            let data = encode_words($sig, $words);
            let g = call(&mut gdb, gfac, data.clone());
            let s = call(&mut sdb, sfac, data);
            assert_eq!(g.success, s.success, "success differs for {}", $sig);
            g
        }};
    }

    let r = both!("make(uint256)", &[word_u256(U256::from(42u64))]);
    assert!(r.success, "make failed: {:?}", r.output);
    let child = addr_from_word(&r.output);
    assert_ne!(child, Address::ZERO, "should have returned a real address");

    // A live contract, not just an address.
    let info = gdb.basic(child).unwrap().expect("child should exist");
    assert!(
        info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "the deployed child must have runtime code"
    );

    // Its constructor ran, with the factory as the caller.
    let g = call(&mut gdb, child, selector("get()").to_vec());
    assert!(g.success && g.output == word_u256(U256::from(42u64)).to_vec(), "constructor arg should be stored");
    let p = call(&mut gdb, child, selector("parent_of()").to_vec());
    assert_eq!(addr_from_word(&p.output), gfac, "the factory should be the child's msg.sender");

    // The factory's own state updated, and it remembered the address.
    both!("count()", &[]);
    assert_eq!(storage(&mut gdb, gfac, 0), U256::from(1u64));
    assert_eq!(addr_from_word(&call(&mut gdb, gfac, selector("last()").to_vec()).output), child);

    // Each call makes a distinct child (CREATE bumps the factory's nonce).
    let r2 = both!("make(uint256)", &[word_u256(U256::from(7u64))]);
    let child2 = addr_from_word(&r2.output);
    assert_ne!(child2, child, "a second make() must deploy a distinct contract");
    let g = call(&mut gdb, child2, selector("get()").to_vec());
    assert_eq!(g.output, word_u256(U256::from(7u64)).to_vec(), "the second child gets its own arg");
    // ...and the first is untouched.
    let g = call(&mut gdb, child, selector("get()").to_vec());
    assert_eq!(g.output, word_u256(U256::from(42u64)).to_vec(), "the first child must be unaffected");

    // gum and Solidity agree on the addresses too: CREATE is
    let s = call(&mut sdb, sfac, encode_words("make(uint256)", &[word_u256(U256::from(1u64))]));
    assert!(s.success);
}

// A child taking a String constructor argument, deployed by a factory. The
const GUM_NEW_DYN: &str = include_str!("fixtures/gum_new_dyn.gum");

const SOL_NEW_DYN: &str = include_str!("fixtures/sol_new_dyn.sol");

#[test]
fn new_contract_passes_dynamic_constructor_args() {
    // A real ERC20-shaped deploy: new Token(name, supply, symbol). The
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gdep = deploy_named(&mut gdb, &solc, GUM_NEW_DYN, "Deployer");
    let sdep = deploy(&mut sdb, sol_creation_bytecode_for(SOL_NEW_DYN, &solc, "Deployer"));

    // Walk the short/long storage-string boundary, and an empty string, so the
    let cases: &[(&[u8], u64, &[u8])] = &[
        (b"Gum Token", 1_000_000, b"GUM"),
        (b"", 0, b""),
        (&[b'x'; 31], 7, &[b'y'; 32]),
        (&[b'z'; 100], u64::MAX, b"Q"),
    ];

    for (name, supply, sym) in cases {
        let data = encode_abi(
            "make(string,uint256,string)",
            &[Arg::Dyn(name), Arg::Static(word_u256(U256::from(*supply))), Arg::Dyn(sym)],
        );
        let g = call(&mut gdb, gdep, data.clone());
        let s = call(&mut sdb, sdep, data);
        assert!(g.success && s.success, "make failed for name len {}: {:?}", name.len(), g.output);

        let gchild = addr_from_word(&g.output);
        let schild = addr_from_word(&s.output);

        // The child decoded exactly what the factory encoded, checked against
        for sig in ["name()", "symbol()", "supply()"] {
            let gr = call(&mut gdb, gchild, selector(sig).to_vec());
            let sr = call(&mut sdb, schild, selector(sig).to_vec());
            assert!(gr.success, "{} failed on the deployed child", sig);
            assert_eq!(gr.output, sr.output, "{} differs for name len {}", sig, name.len());
        }

        // ...and the storage the child wrote matches Solidity's slot-for-slot,
        for slot in 0..3u64 {
            assert_eq!(
                storage(&mut gdb, gchild, slot),
                storage(&mut sdb, schild, slot),
                "child slot {} differs for name len {}", slot, name.len()
            );
        }
    }

    // Guard against the whole thing passing on empty strings.
    let data = encode_abi(
        "make(string,uint256,string)",
        &[Arg::Dyn(b"Gum Token"), Arg::Static(word_u256(U256::from(5u64))), Arg::Dyn(b"GUM")],
    );
    let g = call(&mut gdb, gdep, data);
    let child = addr_from_word(&g.output);
    let out = call(&mut gdb, child, selector("name()").to_vec()).output;
    assert!(
        String::from_utf8_lossy(&out).contains("Gum Token"),
        "the name should have survived the deploy, got {:?}",
        out
    );
}

#[test]
fn new_contract_bubbles_a_failing_child_constructor() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = "\
enum Errors:
    CtorFailed(String reason)

contract Child:
    u256 v

    fn new(u256 x):
        assert(x > 0, Errors.CtorFailed(\"child: x must be positive\"))
        self.v = x

    export fn get() -> u256:
        return Child.v

contract Factory:
    u256 n

    export fn make(u256 x) -> Account:
        return new Child(x)
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let fac = deploy_named(&mut db, &solc, src, "Factory");

    assert!(call(&mut db, fac, encode_words("make(uint256)", &[word_u256(U256::from(5u64))])).success);

    let r = call(&mut db, fac, encode_words("make(uint256)", &[word_u256(U256::ZERO)]));
    assert!(!r.success, "a reverting child constructor must fail the deploy, not return address 0");
    assert!(
        String::from_utf8_lossy(&r.output).contains("child: x must be positive"),
        "the child constructor's own reason must bubble up, got {:?}",
        r.output
    );
}

#[test]
fn a_deployment_cycle_is_rejected() {
    // A's code contains B's, which contains A's... The bytecode would have no
    // fixed point, so this must be a compile error rather than a hang.
    let src = "\
contract A:
    u256 x

    export fn make() -> Account:
        return new B()

contract B:
    u256 y

    export fn make() -> Account:
        return new A()
";
    let (ok, output) = run_gumc_exec(src);
    assert!(!ok, "expected a compile failure, got:\n{}", output);
    assert!(
        output.contains("Deployment cycle"),
        "expected a deployment-cycle diagnostic, got:\n{}",
        output
    );
}

// Dynamic arrays across the ABI boundary, as arguments and as returns. u8[]
// is the interesting one: the wire gives every element a full 32-byte word,
// while gum's memory packs them at 1 byte each, so decode and encode have to
// convert, not copy.
const GUM_ARR_ABI: &str = include_str!("fixtures/gum_arr_abi.gum");

const SOL_ARR_ABI: &str = include_str!("fixtures/sol_arr_abi.sol");

#[test]
fn array_abi_args_and_returns_match_solidity() {
    // [T] used to decode as a scalar: the head word is an offset, and the
    // body got handed that offset as if it were the array pointer, while the
    // published ABI said uint256[], so callers encoded it properly. Silent
    // garbage. Every case here is diffed against Solidity's own encoding.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    // --rich-reverts, so an out-of-bounds index emits the same Panic(0x32) data
    // Solidity does and can be compared byte-for-byte, not merely both-failed.
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_ARR_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_ARR_ABI, &solc));

    macro_rules! both {
        ($sig:expr, $args:expr) => {{
            let data = encode_abi($sig, $args);
            let gr = call(&mut gdb, g, data.clone());
            let sr = call(&mut sdb, s, data);
            assert_eq!(gr.success, sr.success, "success differs for {}", $sig);
            assert_eq!(gr.output, sr.output, "output differs for {}", $sig);
            gr
        }};
    }

    let words = |v: &[u64]| -> Vec<U256> { v.iter().map(|x| U256::from(*x)).collect() };

    // Empty, one element, and past a 32-element boundary (where a u8[]'s packed
    // memory crosses a word).
    for case in [
        vec![],
        vec![7u64],
        vec![1, 2, 3],
        (0..32u64).collect::<Vec<_>>(),
        (0..33u64).collect::<Vec<_>>(),
        (0..70u64).collect::<Vec<_>>(),
    ] {
        let w = words(&case);
        both!("sum(uint256[])", &[Arg::Arr(&w)]);
        both!("echo(uint256[])", &[Arg::Arr(&w)]);
        both!("len_of(uint256[])", &[Arg::Arr(&w)]);

        // u8[]: same counts, values kept in range so both sides agree.
        let w8 = words(&case.iter().map(|x| x % 256).collect::<Vec<_>>());
        both!("sum8(uint8[])", &[Arg::Arr(&w8)]);
        both!("echo8(uint8[])", &[Arg::Arr(&w8)]);

        // Indexing, including out of bounds on the last one.
        if !case.is_empty() {
            both!("at(uint256[],uint256)", &[Arg::Arr(&w), Arg::Static(word_u256(U256::from(case.len() as u64 - 1)))]);
        }
        let r = both!("at(uint256[],uint256)", &[Arg::Arr(&w), Arg::Static(word_u256(U256::from(case.len() as u64)))]);
        assert!(!r.success, "index {} of a {}-element array must revert", case.len(), case.len());
    }

    // Two dynamic args with a static one between them: the tail offsets have to
    // be computed, not assumed.
    let a = words(&[10, 20, 30]);
    let b = words(&[1, 2]);
    let r = both!(
        "two(uint256[],uint256,uint8[])",
        &[Arg::Arr(&a), Arg::Static(word_u256(U256::from(5u64))), Arg::Arr(&b)]
    );
    assert!(r.success);
    assert_eq!(r.output, word_u256(U256::from(68u64)).to_vec(), "10+20+30+5+1+2");

    // Guard against everything passing on empty input.
    let w = words(&[1, 2, 3]);
    let r = call(&mut gdb, g, encode_abi("sum(uint256[])", &[Arg::Arr(&w)]));
    assert_eq!(r.output, word_u256(U256::from(6u64)).to_vec(), "sum([1,2,3]) should be 6");
}

// A child whose constructor takes an array, deployed by a factory: the factory
const GUM_NEW_ARR: &str = include_str!("fixtures/gum_new_arr.gum");

const SOL_NEW_ARR: &str = include_str!("fixtures/sol_new_arr.sol");

#[test]
fn new_contract_passes_array_constructor_args() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gm = deploy_named(&mut gdb, &solc, GUM_NEW_ARR, "Maker");
    let sm = deploy(&mut sdb, sol_creation_bytecode_for(SOL_NEW_ARR, &solc, "Maker"));

    for case in [vec![], vec![5u64], vec![1, 2, 3, 4], (0..40u64).collect::<Vec<_>>()] {
        let w: Vec<U256> = case.iter().map(|x| U256::from(*x)).collect();
        let data = encode_abi(
            "make(uint256[],uint256)",
            &[Arg::Arr(&w), Arg::Static(word_u256(U256::from(100u64)))],
        );
        let gr = call(&mut gdb, gm, data.clone());
        let sr = call(&mut sdb, sm, data);
        assert!(gr.success && sr.success, "make failed for len {}: {:?}", case.len(), gr.output);

        let gchild = addr_from_word(&gr.output);
        let schild = addr_from_word(&sr.output);
        // The child summed exactly what the factory encoded.
        for slot in 0..2u64 {
            assert_eq!(
                storage(&mut gdb, gchild, slot),
                storage(&mut sdb, schild, slot),
                "child slot {} differs for array len {}", slot, case.len()
            );
        }
        let expected: u64 = 100 + case.iter().sum::<u64>();
        assert_eq!(storage(&mut gdb, gchild, 0), U256::from(expected), "total for len {}", case.len());
        assert_eq!(storage(&mut gdb, gchild, 1), U256::from(case.len() as u64), "count");
    }
}

// Fixed arrays across the ABI. T[N] is a static type on the wire: N inline
const GUM_FARR_ABI: &str = include_str!("fixtures/gum_farr_abi.gum");

const SOL_FARR_ABI: &str = include_str!("fixtures/sol_farr_abi.sol");

#[test]
fn fixed_array_abi_matches_solidity() {
    // [u256; 3] used to work by luck, a flat calldatacopy is right only
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_FARR_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_FARR_ABI, &solc));

    macro_rules! both {
        ($sig:expr, $words:expr) => {{
            let data = encode_words($sig, $words);
            let gr = call(&mut gdb, g, data.clone());
            let sr = call(&mut sdb, s, data);
            assert_eq!(gr.success, sr.success, "success differs for {}", $sig);
            assert_eq!(gr.output, sr.output, "output differs for {}", $sig);
            gr
        }};
    }

    // A fixed array's elements sit inline in the head, no offset word.
    let w = |v: u64| word_u256(U256::from(v));
    let r = both!("sum3(uint256[3])", &[w(10), w(20), w(30)]);
    assert!(r.success);
    assert_eq!(r.output, word_u256(U256::from(60u64)).to_vec(), "10+20+30");

    both!("echo3(uint256[3])", &[w(10), w(20), w(30)]);
    both!("sum3_8(uint8[3])", &[w(1), w(2), w(3)]);
    both!("echo3_8(uint8[3])", &[w(1), w(2), w(3)]);
    both!("mixed(uint256,uint8[3],uint256)", &[w(100), w(1), w(2), w(3), w(200)]);

    // The narrow case is the one that was broken: prove it carries real values.
    let r = both!("sum3_8(uint8[3])", &[w(7), w(8), w(9)]);
    assert_eq!(r.output, word_u256(U256::from(24u64)).to_vec(), "7+8+9");
    let r = both!("mixed(uint256,uint8[3],uint256)", &[w(100), w(1), w(2), w(3), w(200)]);
    assert_eq!(r.output, word_u256(U256::from(306u64)).to_vec(), "100+200+1+2+3");

    // Short calldata must revert: a fixed array needs all N words present.
    let r = call(&mut gdb, g, encode_words("sum3(uint256[3])", &[w(1), w(2)]));
    assert!(!r.success, "a [u256; 3] argument with only 2 words must revert");
}

// Transient storage (EIP-1153) with the full collection surface: a scalar, a
const GUM_TRANSIENT: &str = include_str!("fixtures/gum_transient.gum");

#[test]
fn transient_fields_hold_within_a_transaction() {
    // The whole point: a transient field is real storage during the call ,
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(GUM_TRANSIENT, &solc, false));
    let who = Address::from([0x33u8; 20]);

    // 22 + 2002 + 2 (len) + 9 ("Gum Token".length) + 202 + 303 = 2540
    let data = encode_abi(
        "write_then_read(address,string)",
        &[Arg::Static(word_addr(who)), Arg::Dyn(b"Gum Token")],
    );
    let r = call(&mut db, addr, data);
    assert!(r.success, "write_then_read failed: {:?}", r.output);
    assert_eq!(
        r.output,
        word_u256(U256::from(2540u64)).to_vec(),
        "every transient kind must read back what was just written to it"
    );
}

#[test]
fn transient_fields_clear_at_the_end_of_the_transaction() {
    // ...and the whole catch: transient storage clears when the transaction
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(GUM_TRANSIENT, &solc, false));
    let who = Address::from([0x33u8; 20]);

    let data = encode_abi("set_all(address,string)", &[Arg::Static(word_addr(who)), Arg::Dyn(b"hello")]);
    assert!(call(&mut db, addr, data).success, "set_all failed");

    // Each call here is its own transaction, so the transient half is gone.
    let z = word_u256(U256::ZERO).to_vec();
    for (sig, what) in [
        ("read_t()", "a transient scalar"),
        ("tarr_len()", "a transient array's length"),
        ("tmap_of(address)", "a transient mapping entry"),
    ] {
        let d = if sig.contains("address") {
            encode_words(sig, &[word_addr(who)])
        } else {
            selector(sig).to_vec()
        };
        let r = call(&mut db, addr, d);
        assert!(r.success, "{} failed", sig);
        assert_eq!(r.output, z, "{} must be empty in a later transaction", what);
    }
    // The transient string too, empty, not stale bytes.
    let r = call(&mut db, addr, selector("tstr_of()").to_vec());
    assert!(r.success);
    assert_eq!(
        r.output,
        encode_abi_return_string(b""),
        "a transient string must be empty in a later transaction"
    );

    // The persistent half is untouched, proving the two keyspaces are separate
    let r = call(&mut db, addr, selector("read_a()").to_vec());
    assert_eq!(r.output, word_u256(U256::from(11u64)).to_vec(), "the persistent scalar must survive");
    let r = call(&mut db, addr, selector("parr_len()").to_vec());
    assert_eq!(r.output, word_u256(U256::from(1u64)).to_vec(), "the persistent array must survive");
    let r = call(&mut db, addr, encode_words("pmap_of(address)", &[word_addr(who)]));
    assert_eq!(r.output, word_u256(U256::from(1001u64)).to_vec(), "the persistent map must survive");
    let r = call(&mut db, addr, selector("pstr_of()").to_vec());
    assert_eq!(r.output, encode_abi_return_string(b"hello"), "the persistent string must survive");
}

#[test]
fn transient_and_persistent_slots_do_not_collide() {
    // A transient field and a persistent one can share a slot number, they are
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(GUM_TRANSIENT, &solc, false));
    let who = Address::from([0x33u8; 20]);

    let data = encode_abi("set_all(address,string)", &[Arg::Static(word_addr(who)), Arg::Dyn(b"hi")]);
    assert!(call(&mut db, addr, data).success);

    // The transient writes used slots 0..3 of the transient keyspace. If they
    let persistent: Vec<U256> = (0..4u64).map(|s| storage(&mut db, addr, s)).collect();
    for v in [22u64, 202, 2002] {
        assert!(
            !persistent.contains(&U256::from(v)),
            "a transient value ({}) leaked into persistent storage: {:?}", v, persistent
        );
    }
    // ...and the persistent values really are there, so this isn't vacuous.
    assert!(persistent.contains(&U256::from(11u64)), "the persistent scalar should be in storage: {:?}", persistent);
}

const SOL_ATTACKER: &str = include_str!("fixtures/sol_attacker.sol");

#[test]
fn reentrancy_guard_blocks_a_real_attack_and_unsafe_opts_out() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };

    // Guarded (default): the re-entrant poke() must hit the lock and revert,
    {
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let vault = deploy(&mut db, gum_creation_bytecode(&GUM_REENTRANT.replace("{MOD}", ""), &solc, false));
        let atk = deploy(&mut db, sol_creation_bytecode(SOL_ATTACKER, &solc));
        let set = encode_words("setTarget(address)", &[word_addr(vault)]);
        assert!(call(&mut db, atk, set).success, "setTarget failed");

        let r = call(&mut db, vault, encode_words("poke(address)", &[word_addr(atk)]));
        assert!(!r.success, "reentrancy guard must block the attack, but poke() succeeded");
        assert_eq!(storage(&mut db, vault, 0), U256::ZERO, "guarded: reverted call must leave counter at 0");
    }

    // unsafe opts out: the same attack now goes through, proving the guard is
    {
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let vault = deploy(&mut db, gum_creation_bytecode(&GUM_REENTRANT.replace("{MOD}", "unsafe "), &solc, false));
        let atk = deploy(&mut db, sol_creation_bytecode(SOL_ATTACKER, &solc));
        let set = encode_words("setTarget(address)", &[word_addr(vault)]);
        assert!(call(&mut db, atk, set).success, "setTarget failed");

        let r = call(&mut db, vault, encode_words("poke(address)", &[word_addr(atk)]));
        assert!(r.success, "unsafe fn should permit reentrancy, but the call failed");
        assert_eq!(
            storage(&mut db, vault, 0),
            U256::from(2u64),
            "unsafe: the re-entrant call should have bumped counter twice"
        );
    }
}

#[test]
fn reentrancy_lock_does_not_leak_across_calls_in_one_transaction() {
    // Transient storage persists for the whole transaction, so a guarded call
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let gum_src = "\
use gum.defaults.Account

interface ICallback:
    fn ping() -> bool

contract V:
    u256 counter

    export fn bump():
        V.counter = V.counter + 1

    export fn poke(Account cb):
        assert(ICallback(cb).ping(), \"cb failed\")
";
    let addr = deploy(&mut db, gum_creation_bytecode(gum_src, &solc, false));
    // Two back-to-back guarded calls must both succeed.
    for i in 1..=3u64 {
        let r = call(&mut db, addr, selector("bump()").to_vec());
        assert!(r.success, "call {} must succeed, the lock must not survive the previous call", i);
        assert_eq!(storage(&mut db, addr, 0), U256::from(i), "counter after call {}", i);
    }
}

#[test]
fn account_pay_transfers_eth() {
    // to.pay(amount) must actually move ETH, and hand back the verdict.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum_src = "\
use gum.defaults.Account
use gum.defaults.Message

contract V:
    u256 total

    export payable fn deposit():
        V.total = V.total + Message.value()

    export fn withdraw(Account to, u256 amt):
        V.total = V.total - amt
        assert(to.pay(amt), \"pay failed\")
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let vault = deploy(&mut db, gum_creation_bytecode(gum_src, &solc, false));

    let wei = U256::from(5_000u64);
    let r = call_with_value(&mut db, deployer(), vault, selector("deposit()").to_vec(), wei);
    assert!(r.success, "payable deposit failed");
    assert_eq!(db.basic(vault).unwrap().unwrap().balance, wei, "vault should hold the deposit");

    let payee = Address::from([0x77u8; 20]);
    let before = db.basic(payee).unwrap().map(|a| a.balance).unwrap_or_default();
    let data = encode_words("withdraw(address,uint256)", &[word_addr(payee), word_u256(U256::from(2_000u64))]);
    assert!(call(&mut db, vault, data).success, "withdraw failed");

    let after = db.basic(payee).unwrap().unwrap().balance;
    assert_eq!(after - before, U256::from(2_000u64), "pay() must actually transfer the ETH");
    assert_eq!(db.basic(vault).unwrap().unwrap().balance, U256::from(3_000u64), "vault balance must drop");
    assert_eq!(storage(&mut db, vault, 0), U256::from(3_000u64), "accounting slot must match");
}

#[test]
fn account_transfer_sends_eth_and_reverts_on_failure() {
    // to.transfer(amount) is the checked companion to pay(): it moves the ETH
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    use revm::bytecode::Bytecode;
    use revm::state::AccountInfo;

    let gum_src = "\
use gum.defaults.Account
use gum.defaults.Message

contract V:
    u256 total

    export payable fn deposit():
        V.total = V.total + Message.value()

    export fn send(Account to, u256 amt):
        V.total = V.total - amt
        to.transfer(amt)
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let vault = deploy(&mut db, gum_creation_bytecode(gum_src, &solc, false));

    let wei = U256::from(5_000u64);
    assert!(call_with_value(&mut db, deployer(), vault, selector("deposit()").to_vec(), wei).success);

    // Happy path: a plain EOA takes the money.
    let payee = Address::from([0x77u8; 20]);
    let data = encode_words("send(address,uint256)", &[word_addr(payee), word_u256(U256::from(2_000u64))]);
    assert!(call(&mut db, vault, data).success, "transfer to a plain EOA should succeed");
    assert_eq!(db.basic(payee).unwrap().unwrap().balance, U256::from(2_000u64), "transfer() must actually move the ETH");
    assert_eq!(db.basic(vault).unwrap().unwrap().balance, U256::from(3_000u64), "vault balance must drop");
    assert_eq!(storage(&mut db, vault, 0), U256::from(3_000u64));

    // Sad path: a recipient that rejects the money, reverting with 0xdeadbeef.
    let mut code = vec![0x7fu8];
    code.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    code.extend_from_slice(&[0u8; 28]);
    code.extend_from_slice(&[0x5f, 0x52, 0x60, 0x04, 0x5f, 0xfd]);
    let bc = Bytecode::new_raw(code.into());
    let rejector = Address::from([0x88u8; 20]);
    let mut info = AccountInfo::default();
    info.code_hash = bc.hash_slow();
    info.code = Some(bc);
    db.insert_account_info(rejector, info);

    let data = encode_words("send(address,uint256)", &[word_addr(rejector), word_u256(U256::from(1_000u64))]);
    let r = call(&mut db, vault, data);
    assert!(!r.success, "transfer() to a rejecting recipient must revert, not silently continue");
    assert_eq!(r.output, vec![0xde, 0xad, 0xbe, 0xef], "the recipient's own revert data must be bubbled up");

    // And the revert must have rolled back the bookkeeping that ran before it.
    assert_eq!(storage(&mut db, vault, 0), U256::from(3_000u64), "failed transfer must not have debited the total");
    assert_eq!(db.basic(vault).unwrap().unwrap().balance, U256::from(3_000u64), "vault must still hold the ETH");
}




const GUM_P256: &str = include_str!("fixtures/gum_p256.gum");

#[test]
fn p256_verify_accepts_a_real_signature_and_rejects_tampering() {
    // A genuine secp256r1 signature, verified through gum's Crypto.verify_p256
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut gdb, gum_creation_bytecode(GUM_P256, &solc, false));

    // Deterministic key so the test can't flake.
    let sk = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
    let vk = sk.verifying_key();
    let pt = vk.to_encoded_point(false);
    let qx = U256::from_be_slice(pt.x().unwrap());
    let qy = U256::from_be_slice(pt.y().unwrap());

    let msg_hash = [0x9au8; 32];
    let sig: Signature = sk.sign_prehash(&msg_hash).unwrap();
    let r = U256::from_be_slice(&sig.r().to_bytes());
    let s = U256::from_be_slice(&sig.s().to_bytes());
    let h = U256::from_be_slice(&msg_hash);

    let sig = "v(uint256,uint256,uint256,uint256,uint256)";

    // The real signature must verify.
    let good = call(&mut gdb, addr, encode(sig, &[h, r, s, qx, qy]));
    assert!(good.success, "verify_p256 reverted");
    assert_eq!(
        U256::from_be_slice(&good.output),
        U256::from(1u64),
        "a valid P-256 signature must verify (is the 0x100 precompile active?)"
    );

    // Every single-field tamper must be rejected, otherwise "returns true" is
    let tampered: &[(&str, [U256; 5])] = &[
        ("wrong hash", [h ^ U256::from(1u64), r, s, qx, qy]),
        ("wrong r", [h, r ^ U256::from(1u64), s, qx, qy]),
        ("wrong s", [h, r, s ^ U256::from(1u64), qx, qy]),
        ("wrong qx", [h, r, s, qx ^ U256::from(1u64), qy]),
        ("wrong qy", [h, r, s, qx, qy ^ U256::from(1u64)]),
        ("all zero", [U256::ZERO; 5]),
    ];
    for (what, args) in tampered {
        let bad = call(&mut gdb, addr, encode(sig, args));
        assert!(bad.success, "{}: must return false, not revert", what);
        assert_eq!(U256::from_be_slice(&bad.output), U256::ZERO, "{}: must NOT verify", what);
    }
}

#[test]
fn eip7702_delegated_to_reads_the_delegation_indicator() {
    // A 7702-delegated EOA's code is exactly 0xef0100 ++ <20-byte target>.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    use revm::bytecode::Bytecode;
    use revm::state::AccountInfo;

    let gum_src = "\
use gum.defaults.Account

contract App:
    export fn deleg(Account a) -> Account:
        return a.delegated_to()

    export fn is_deleg(Account a) -> bool:
        return a.is_delegated()
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(gum_src, &solc, false));

    // An EOA delegated to target, built the way revm itself represents 7702.
    let target = Address::from([0xabu8; 20]);
    let bc = Bytecode::new_eip7702(target);
    let delegated = Address::from([0x42u8; 20]);
    let mut info = AccountInfo::default();
    info.code_hash = bc.hash_slow();
    info.code = Some(bc);
    db.insert_account_info(delegated, info);

    // A plain EOA with no code at all.
    let plain = Address::from([0x43u8; 20]);
    db.insert_account_info(plain, AccountInfo::default());

    // (account, expected delegation target, expected is_delegated)
    let cases: &[(Address, Address, bool)] = &[
        (delegated, target, true),
        (plain, Address::ZERO, false),
        (addr, Address::ZERO, false), // an ordinary contract is not delegated
    ];
    for (who, want, want_flag) in cases {
        let r = call(&mut db, addr, encode_words("deleg(address)", &[word_addr(*who)]));
        assert!(r.success, "deleg() reverted for {:?}", who);
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from_be_bytes(word_addr(*want)),
            "delegated_to({:?}) wrong", who
        );
        let f = call(&mut db, addr, encode_words("is_deleg(address)", &[word_addr(*who)]));
        assert!(f.success, "is_deleg() reverted");
        assert_eq!(
            U256::from_be_slice(&f.output) == U256::from(1u64),
            *want_flag,
            "is_delegated({:?}) wrong", who
        );
    }
}

const GUM_CRYPTO: &str = include_str!("fixtures/gum_crypto.gum");

const SOL_CRYPTO: &str = include_str!("fixtures/sol_crypto.sol");

#[test]
fn keccak256_and_ecrecover_match_solidity() {
    // Both were declared in the stdlib but never wired to codegen: keccak256
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_CRYPTO, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_CRYPTO, &solc));

    // keccak256 must hash the contents, not the header or the pointer ,
    for msg in [
        b"".as_ref(),
        b"a".as_ref(),
        b"hello world".as_ref(),
        b"a string that is definitely longer than thirty-two bytes".as_ref(),
    ] {
        let data = encode_abi("hash_str(string)", &[Arg::Dyn(msg)]);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert!(g.success && s.success, "hash_str reverted for {:?}", msg);
        assert_eq!(g.output, s.output, "keccak256 differs from Solidity for {:?}", msg);
    }

    // ecrecover against a real secp256k1 signature: gum must recover the same
    let sig = "rec(uint256,uint8,uint256,uint256)";
    let cases: &[[U256; 4]] = &[
        // A genuine mainnet-style signature triple.
        [
            U256::from_be_slice(&hex::decode("bb1a0f1b0e0b0d6b5a8b1c0f2c4a5e6d7c8b9a0f1e2d3c4b5a6978869504f3e2").unwrap()),
            U256::from(27u64),
            U256::from_be_slice(&hex::decode("6b8a3f2f1e0d9c8b7a695847362514039281706f5e4d3c2b1a09f8e7d6c5b4a3").unwrap()),
            U256::from_be_slice(&hex::decode("1c2d3e4f50617283940516273849506172839405162738495061728394051627").unwrap()),
        ],
        // Garbage -> both must return the zero address, not revert.
        [U256::ZERO, U256::from(27u64), U256::ZERO, U256::ZERO],
        // Invalid v -> failure path.
        [U256::from(1u64), U256::from(99u64), U256::from(2u64), U256::from(3u64)],
    ];
    for args in cases {
        let data = encode(sig, args);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert_eq!(g.success, s.success, "ecrecover success differs");
        assert_eq!(g.output, s.output, "ecrecover result differs from Solidity for {:?}", args);
    }
}

const GUM_SSTR: &str = include_str!("fixtures/gum_sstr.gum");

const SOL_SSTR: &str = include_str!("fixtures/sol_sstr.sol");

#[test]
fn storage_string_layout_matches_solidity() {
    // The thing standing between gum and a real ERC20 name/symbol.
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_SSTR, &solc, false));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_SSTR, &solc));

    // Walk the short/long boundary deliberately, and end shorter than we
    let cases: &[&[u8]] = &[
        b"",
        b"a",
        b"Gum Token",
        &[b'x'; 31],  // longest short form
        &[b'y'; 32],  // first long form
        &[b'z'; 33],
        &[b'w'; 100], // spans four data slots
        b"back to short",
        b"",
    ];

    for name in cases {
        let data = encode_abi("set_name(string)", &[Arg::Dyn(name)]);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert!(g.success && s.success, "set_name failed for len {}", name.len());

        // The header slot must be identical.
        assert_eq!(
            storage(&mut gdb, gaddr, 0),
            storage(&mut sdb, saddr, 0),
            "storage string header slot differs for len {}", name.len()
        );
        // ...and every data slot at keccak256(0), including the ones a previous
        let base = dyn_array_data_base(0);
        for i in 0..5u64 {
            let slot = base + U256::from(i);
            assert_eq!(
                storage_at(&mut gdb, gaddr, slot),
                storage_at(&mut sdb, saddr, slot),
                "storage string data slot {} differs for len {}", i, name.len()
            );
        }
        // And reading it back must round-trip identically.
        let g = call(&mut gdb, gaddr, selector("name_of()").to_vec());
        let s = call(&mut sdb, saddr, selector("name_of()").to_vec());
        assert!(g.success && s.success, "name_of failed for len {}", name.len());
        assert_eq!(g.output, s.output, "name_of round-trip differs for len {}", name.len());
        assert_eq!(g.output, abi_encode_string(name), "name_of must return the value we set");
    }

    // The neighbouring scalar must be untouched by all that: a storage string
    let d = encode("set_supply(uint256)", &[U256::from(4242u64)]);
    assert!(call(&mut gdb, gaddr, d.clone()).success);
    assert!(call(&mut sdb, saddr, d).success);
    assert_eq!(storage(&mut gdb, gaddr, 1), U256::from(4242u64), "supply must live in its own slot");
    assert_eq!(storage(&mut gdb, gaddr, 1), storage(&mut sdb, saddr, 1), "supply slot differs");
}

// A String value in a mapping: HashMap(Account, String). The value lives at the
// mapping value slot keccak256(key ‖ p), which doubles as the string's base slot
// (short packed inline, long at keccak256(valueSlot)), exactly as Solidity lays
// out mapping(address => string). Diffs the value slot, the data region, the
// round-trip read, key isolation, and delete against a Solidity twin, across the
// short/long boundary.
#[test]
fn mapping_string_value_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/gum_map_string.gum");
    let sol_src = include_str!("fixtures/sol_map_string.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    // keccak256 over a value slot, for the long-string data region.
    let str_data_base = |value_slot: U256| -> U256 {
        use tiny_keccak::{Hasher, Keccak};
        let mut k = Keccak::v256();
        let mut out = [0u8; 32];
        k.update(&value_slot.to_be_bytes::<32>());
        k.finalize(&mut out);
        U256::from_be_bytes(out)
    };

    let alice = Address::from([0xa1u8; 20]);
    let bob = Address::from([0xb0u8; 20]);

    // Walk the short/long boundary, and shrink back down at the end so the
    // long-form data slots must be released, exactly as the field case does.
    let cases: &[&[u8]] = &[
        b"",
        b"a",
        b"Alice",
        &[b'x'; 31], // longest short form
        &[b'y'; 32], // first long form
        &[b'z'; 100], // spans four data slots
        b"short again",
        b"",
    ];

    for name in cases {
        let data = encode_abi("set(address,string)", &[Arg::Static(word_addr(alice)), Arg::Dyn(name)]);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert!(g.success && s.success, "set failed for len {}", name.len());

        // The mapping value slot (string header) must be identical.
        let vslot = mapping_slot(alice, 0);
        assert_eq!(
            storage_at(&mut gdb, ga, vslot),
            storage_at(&mut sdb, sa, vslot),
            "value/header slot differs for len {}", name.len()
        );
        // ...and every data slot at keccak256(valueSlot), including any a longer
        // previous value wrote that a shorter one must now have cleared.
        let base = str_data_base(vslot);
        for i in 0..5u64 {
            let slot = base + U256::from(i);
            assert_eq!(
                storage_at(&mut gdb, ga, slot),
                storage_at(&mut sdb, sa, slot),
                "data slot {} differs for len {}", i, name.len()
            );
        }
        // And the value must round-trip identically, and equal what we set.
        let rd = encode_abi("get(address)", &[Arg::Static(word_addr(alice))]);
        let g = call(&mut gdb, ga, rd.clone());
        let s = call(&mut sdb, sa, rd);
        assert!(g.success && s.success, "get failed for len {}", name.len());
        assert_eq!(g.output, s.output, "get round-trip differs for len {}", name.len());
        assert_eq!(g.output, abi_encode_string(name), "get must return the value we set");
    }

    // A second key must be independent: writing bob leaves alice untouched, and
    // both slots match Solidity.
    let bob_name: &[u8] = b"Bob the builder, a long enough name to go long-form for sure yes";
    let data = encode_abi("set(address,string)", &[Arg::Static(word_addr(bob)), Arg::Dyn(bob_name)]);
    assert!(call(&mut gdb, ga, data.clone()).success);
    assert!(call(&mut sdb, sa, data).success);
    for who in [alice, bob] {
        let vslot = mapping_slot(who, 0);
        assert_eq!(
            storage_at(&mut gdb, ga, vslot),
            storage_at(&mut sdb, sa, vslot),
            "value slot differs for {:?}", who
        );
    }

    // delete releases the slot region exactly as Solidity's delete does.
    let del = encode_abi("clear(address)", &[Arg::Static(word_addr(bob))]);
    assert!(call(&mut gdb, ga, del.clone()).success);
    assert!(call(&mut sdb, sa, del).success);
    let vslot = mapping_slot(bob, 0);
    assert_eq!(storage_at(&mut gdb, ga, vslot), U256::ZERO, "value slot not cleared");
    let base = str_data_base(vslot);
    for i in 0..3u64 {
        let slot = base + U256::from(i);
        assert_eq!(
            storage_at(&mut gdb, ga, slot),
            storage_at(&mut sdb, sa, slot),
            "data slot {} not released like Solidity", i
        );
    }
}

// A dynamic array as a mapping value: HashMap(Account, [u256]). m[k] lives at the
// mapping value slot keccak256(key ‖ p), which holds the length; the elements
// pack from keccak256(that slot), exactly as Solidity lays out
// mapping(address => uint256[]). Diffs the length slot, the element data slots,
// the get/size reads, key isolation and delete against a Solidity twin across a
// push/set/pop/delete sequence.
#[test]
fn mapping_dynamic_array_value_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/gum_map_dyn_arr.gum");
    let sol_src = include_str!("fixtures/sol_map_dyn_arr.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let arr_base = |value_slot: U256| -> U256 {
        use tiny_keccak::{Hasher, Keccak};
        let mut k = Keccak::v256();
        let mut out = [0u8; 32];
        k.update(&value_slot.to_be_bytes::<32>());
        k.finalize(&mut out);
        U256::from_be_bytes(out)
    };

    let alice = Address::from([0xa1u8; 20]);
    let bob = Address::from([0xb0u8; 20]);

    // A push/set/pop/delete script across two keys. Each step runs on both, then
    // the length slot, the first few element slots, and the read-backs are diffed.
    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (alice, "add(address,uint256)", vec![word_u256(U256::from(10u64))]),
        (alice, "add(address,uint256)", vec![word_u256(U256::from(20u64))]),
        (alice, "add(address,uint256)", vec![word_u256(U256::from(30u64))]),
        (bob, "add(address,uint256)", vec![word_u256(U256::from(99u64))]),
        (alice, "set(address,uint256,uint256)", vec![word_addr(alice), word_u256(U256::from(1u64)), word_u256(U256::from(25u64))]),
        (alice, "drop_last(address)", vec![]),
    ];

    for (caller, sig, tail) in &steps {
        // The address is the first arg for every entry here; build calldata as
        // selector + who + the remaining words (which already include who for the
        // multi-arg sigs, so use encode_words for those and prepend who for the rest).
        let data = if sig.starts_with("set") {
            encode_words(sig, tail)
        } else if sig.starts_with("add") {
            let mut w = vec![word_addr(*caller)];
            w.extend_from_slice(tail);
            encode_words(sig, &w)
        } else {
            encode_words(sig, &[word_addr(*caller)])
        };
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "{}: success mismatch", sig);
        assert_eq!(g.output, s.output, "{}: output mismatch", sig);

        for who in [alice, bob] {
            let vslot = mapping_slot(who, 0);
            assert_eq!(
                storage_at(&mut gdb, ga, vslot),
                storage_at(&mut sdb, sa, vslot),
                "{}: length slot for {:?}", sig, who
            );
            let base = arr_base(vslot);
            for i in 0..4u64 {
                let slot = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, ga, slot),
                    storage_at(&mut sdb, sa, slot),
                    "{}: element slot {} for {:?}", sig, i, who
                );
            }
            // size() and each element get() must read back identically.
            let sz = call(&mut gdb, ga, encode_words("size(address)", &[word_addr(who)]));
            let sz2 = call(&mut sdb, sa, encode_words("size(address)", &[word_addr(who)]));
            assert_eq!(sz.output, sz2.output, "{}: size() for {:?}", sig, who);
            let n = U256::from_be_slice(&sz.output).to::<u64>();
            for i in 0..n {
                let gi = call(&mut gdb, ga, encode_words("get(address,uint256)", &[word_addr(who), word_u256(U256::from(i))]));
                let si = call(&mut sdb, sa, encode_words("get(address,uint256)", &[word_addr(who), word_u256(U256::from(i))]));
                assert_eq!(gi.success, si.success, "{}: get({}) success for {:?}", sig, i, who);
                assert_eq!(gi.output, si.output, "{}: get({}) for {:?}", sig, i, who);
            }
        }
    }

    // delete releases the region exactly as Solidity's delete does.
    let del = encode_words("clear(address)", &[word_addr(alice)]);
    assert!(call(&mut gdb, ga, del.clone()).success);
    assert!(call(&mut sdb, sa, del).success);
    let vslot = mapping_slot(alice, 0);
    assert_eq!(storage_at(&mut gdb, ga, vslot), U256::ZERO, "length not cleared");
    let base = arr_base(vslot);
    for i in 0..4u64 {
        let slot = base + U256::from(i);
        assert_eq!(
            storage_at(&mut gdb, ga, slot),
            storage_at(&mut sdb, sa, slot),
            "element slot {} not released like Solidity", i
        );
    }
}

// A String element across the ABI: string[] as an argument and a return, plus
// indexing one element out. gum decodes the calldata blob and re-encodes it
// through the new String element codec (gum_abi_str_cd/put/size) wrapped by the
// existing dynamic-array codec; feeding identical calldata to a Solidity twin
// and diffing the output proves the decode+encode round-trips byte-for-byte.
#[test]
fn string_array_across_the_abi_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/gum_string_array.gum");
    let sol_src = include_str!("fixtures/sol_string_array.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    // Hand-encode a `string[]` as the single argument of `sig`, walking the
    // short/long boundary so element widths and padding both get exercised.
    let encode_str_arr = |sig: &str, items: &[&[u8]]| -> Vec<u8> {
        let mut data = selector(sig).to_vec();
        data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>()); // offset to the array
        let n = items.len();
        let mut table = Vec::new();
        let mut tails = Vec::new();
        let mut cur = n * 32; // offsets are measured from just after the count word
        for it in items {
            table.extend_from_slice(&U256::from(cur).to_be_bytes::<32>());
            let mut e = Vec::new();
            e.extend_from_slice(&U256::from(it.len()).to_be_bytes::<32>());
            e.extend_from_slice(it);
            let pad = (32 - (it.len() % 32)) % 32;
            e.extend(std::iter::repeat(0u8).take(pad));
            cur += e.len();
            tails.extend_from_slice(&e);
        }
        data.extend_from_slice(&U256::from(n).to_be_bytes::<32>()); // count
        data.extend_from_slice(&table);
        data.extend_from_slice(&tails);
        data
    };

    let cases: Vec<Vec<&[u8]>> = vec![
        vec![],
        vec![b"a"],
        vec![b"", b"x"],
        vec![b"Alice", &[b'y'; 32], b"", &[b'z'; 65]],
        vec![&[b'w'; 31], &[b'v'; 33]],
    ];

    for items in &cases {
        let data = encode_str_arr("echo(string[])", items);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "echo success mismatch for {} items", items.len());
        assert!(g.success, "echo reverted for {} items", items.len());
        assert_eq!(g.output, s.output, "echo output differs for {} items", items.len());

        // `xs.length` (memory array of pointer-sized String slots) must match.
        let da = encode_str_arr("alen(string[])", items);
        let g = call(&mut gdb, ga, da.clone());
        let s = call(&mut sdb, sa, da);
        assert_eq!(g.output, s.output, "xs.length differs for {} items", items.len());
        assert_eq!(U256::from_be_slice(&g.output), U256::from(items.len()), "xs.length wrong");

        if !items.is_empty() {
            // `xs[0].length`: index one String element out, then read its length.
            let dp = encode_str_arr("plen(string[])", items);
            let g = call(&mut gdb, ga, dp.clone());
            let s = call(&mut sdb, sa, dp);
            assert_eq!(g.output, s.output, "xs[0].length differs");
            assert_eq!(U256::from_be_slice(&g.output), U256::from(items[0].len()), "xs[0].length wrong");

            // `return xs[0]`: index a String out and return it whole.
            let data = encode_str_arr("first(string[])", items);
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(g.success, s.success, "first success mismatch");
            assert!(g.success, "first reverted");
            assert_eq!(g.output, s.output, "first output differs");
            assert_eq!(g.output, abi_encode_string(items[0]), "first must return element 0");
        }
    }
}

// A struct with a dynamic field across the ABI: Meta { u256 id; String name }.
// It rides the wire as a dynamic tuple — a head of (id, offset-to-name) then the
// name's tail — through the new dynamic-struct codec composing the scalar and
// String field codecs. echo decodes the tuple and re-encodes it; feeding identical
// calldata to a Solidity twin and diffing the output proves it round-trips
// byte-for-byte, and a struct arg + struct return both work.
#[test]
fn dynamic_struct_across_the_abi_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/gum_dyn_struct.gum");
    let sol_src = include_str!("fixtures/sol_dyn_struct.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    // Hand-encode `echo((uint256,string))`: the tuple is dynamic, so it sits
    // behind an offset; inside, the head is (id, offset-to-name), then the name.
    let encode = |id: u64, name: &[u8]| -> Vec<u8> {
        let mut data = selector("echo((uint256,string))").to_vec();
        data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>()); // offset to the tuple
        data.extend_from_slice(&U256::from(id).to_be_bytes::<32>()); // id (head word 0)
        data.extend_from_slice(&U256::from(64u64).to_be_bytes::<32>()); // offset to name within tuple
        data.extend_from_slice(&U256::from(name.len()).to_be_bytes::<32>()); // name length
        let mut nb = name.to_vec();
        let pad = (32 - (name.len() % 32)) % 32;
        nb.extend(std::iter::repeat(0u8).take(pad));
        data.extend_from_slice(&nb);
        data
    };

    let cases: &[(u64, &[u8])] = &[
        (0, b""),
        (7, b"Alice"),
        (42, &[b'x'; 31]),  // longest short
        (100, &[b'y'; 32]), // first long
        (999, &[b'z'; 80]),
    ];
    for (id, name) in cases {
        let data = encode(*id, name);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "echo success for id {} len {}", id, name.len());
        assert!(g.success, "echo reverted for id {} len {}", id, name.len());
        assert_eq!(g.output, s.output, "echo output differs for id {} len {}", id, name.len());
    }
}

// The outbound side of the dynamic-struct codec: a Caller encodes a Meta as an
// interface-call argument (abi_arg_blob) and decodes the Meta it gets back. The
// Target echoes it. Diffing the Caller's output against a Solidity twin exercises
// the whole loop — arg encode, the target's arg decode + return encode, and the
// caller's return decode — end to end.
#[test]
fn dynamic_struct_through_an_interface_call_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/gum_dyn_struct_iface.gum");
    let sol_src = include_str!("fixtures/sol_dyn_struct_iface.sol");

    let gum_target = gum_creation_bytecode_for(gum_src, &solc, false, "Target");
    let gum_caller = gum_creation_bytecode_for(gum_src, &solc, false, "Caller");
    let sol_target = sol_creation_bytecode_for(sol_src, &solc, "Target");
    let sol_caller = sol_creation_bytecode_for(sol_src, &solc, "Caller");

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gt = deploy(&mut gdb, gum_target);
    let gc = deploy(&mut gdb, gum_caller);
    let st = deploy(&mut sdb, sol_target);
    let sc = deploy(&mut sdb, sol_caller);

    let encode = |target: Address, id: u64, name: &[u8]| -> Vec<u8> {
        let mut data = selector("call_it(address,uint256,string)").to_vec();
        data.extend_from_slice(&word_addr(target));
        data.extend_from_slice(&U256::from(id).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(96u64).to_be_bytes::<32>()); // offset to the string (3 head words)
        data.extend_from_slice(&U256::from(name.len()).to_be_bytes::<32>());
        let mut nb = name.to_vec();
        let pad = (32 - (name.len() % 32)) % 32;
        nb.extend(std::iter::repeat(0u8).take(pad));
        data.extend_from_slice(&nb);
        data
    };

    for (id, name) in [(1u64, &b""[..]), (7, b"Alice"), (42, &[b'z'; 70][..])] {
        let g = call(&mut gdb, gc, encode(gt, id, name));
        let s = call(&mut sdb, sc, encode(st, id, name));
        assert_eq!(g.success, s.success, "call_it success for id {} len {}", id, name.len());
        assert!(g.success, "call_it reverted for id {} len {}", id, name.len());
        assert_eq!(g.output, s.output, "call_it output differs for id {} len {}", id, name.len());
    }
}

// Structural differential fuzzer. Instead of running random call *sequences*
// against a fixed contract (fuzz_erc20 etc.), this generates random *contracts* —
// a random set of fields of random scalar types, each with a setter, a getter,
// and (for numeric types) a checked-add mutator — and diffs gum against an
// equivalent Solidity twin. The random field mix produces random storage packing,
// which is exactly where a slot/mask bug hides: gum reorders fields largest-first
// and read-modify-writes packed slots, so a wrong mask or offset corrupts a
// slot-mate. Because both sides receive *identical* calldata (a raw random word,
// not a canonically-encoded value), the encoder need not be canonical — any
// behavioural divergence, including mishandled dirty upper bits, is a real gum bug.
#[test]
fn fuzz_random_storage_layout_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    // (gum type, solidity type, supports checked +)
    let types: &[(&str, &str, bool)] = &[
        ("bool", "bool", false),
        ("u8", "uint8", true),
        ("u16", "uint16", true),
        ("u32", "uint32", true),
        ("u64", "uint64", true),
        ("u128", "uint128", true),
        ("u256", "uint256", true),
        ("i8", "int8", true),
        ("i32", "int32", true),
        ("i128", "int128", true),
        ("i256", "int256", true),
        ("Account", "address", false),
    ];

    for seed in 0..48u64 {
        let mut rng = Rng(0xf022_0000 ^ seed);
        let nfields = 3 + (rng.next_u64() % 6) as usize; // 3..=8 fields
        let fields: Vec<(&str, &str, bool)> =
            (0..nfields).map(|_| types[(rng.next_u64() as usize) % types.len()]).collect();

        // Build the two twins field-by-field with matching setters/getters/adders.
        let mut gum = String::from("contract C:\n");
        for (i, (g, _, _)) in fields.iter().enumerate() {
            gum.push_str(&format!("    {} f{}\n", g, i));
        }
        for (i, (g, _, arith)) in fields.iter().enumerate() {
            gum.push_str(&format!("\n    export fn s{i}({g} v):\n        C.f{i} = v\n", i = i, g = g));
            gum.push_str(&format!("\n    export fn g{i}() -> {g}:\n        return C.f{i}\n", i = i, g = g));
            if *arith {
                gum.push_str(&format!("\n    export fn a{i}({g} v):\n        C.f{i} = C.f{i} + v\n", i = i, g = g));
                gum.push_str(&format!("\n    export fn b{i}({g} v):\n        C.f{i} = C.f{i} - v\n", i = i, g = g));
                // Multiply by a left-position literal, so the overflow bound must
                // come from the field type, not the literal default u256.
                gum.push_str(&format!("\n    export fn m{i}():\n        C.f{i} = 3 * C.f{i}\n", i = i));
            }
        }

        let mut sol = String::from("// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract C {\n");
        for (i, (_, s, _)) in fields.iter().enumerate() {
            sol.push_str(&format!("    {} f{};\n", s, i));
        }
        for (i, (_, s, arith)) in fields.iter().enumerate() {
            sol.push_str(&format!("    function s{i}({s} v) external {{ f{i} = v; }}\n", i = i, s = s));
            sol.push_str(&format!("    function g{i}() external view returns ({s}) {{ return f{i}; }}\n", i = i, s = s));
            if *arith {
                sol.push_str(&format!("    function a{i}({s} v) external {{ f{i} = f{i} + v; }}\n", i = i, s = s));
                sol.push_str(&format!("    function b{i}({s} v) external {{ f{i} = f{i} - v; }}\n", i = i, s = s));
                sol.push_str(&format!("    function m{i}() external {{ f{i} = 3 * f{i}; }}\n", i = i));
            }
        }
        sol.push_str("}\n");

        let gbc = gum_creation_bytecode(&gum, &solc, false);
        let sbc = sol_creation_bytecode(&sol, &solc);
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gbc);
        let sa = deploy(&mut sdb, sbc);

        // A canonical, in-range argument word for the field's type, so both sides
        // accept it (no decode-validation divergence) and the diff isolates the
        // storage/packing/arithmetic paths. Values skew toward type boundaries and
        // near-max to exercise sign bits, mask edges and checked-arithmetic reverts.
        let encode_arg = |rng: &mut Rng, ty: &str| -> [u8; 32] {
            if ty == "bool" {
                let mut w = [0u8; 32];
                w[31] = (rng.next_u64() & 1) as u8;
                return w;
            }
            if ty == "Account" {
                let mut w = [0u8; 32];
                for b in w.iter_mut().skip(12) {
                    *b = (rng.next_u64() & 0xff) as u8;
                }
                return w;
            }
            let signed = ty.starts_with('i');
            let bits: usize = ty[1..].parse().unwrap();
            let raw = rng.next_u256(true);
            let val = if bits >= 256 {
                raw
            } else {
                let mask = (U256::from(1u64) << bits) - U256::from(1u64);
                let low = raw & mask;
                if signed && ((low >> (bits - 1)) & U256::from(1u64)) == U256::from(1u64) {
                    low | !mask // sign-extend a negative into the full word
                } else {
                    low
                }
            };
            val.to_be_bytes::<32>()
        };

        let getters_match = |gdb: &mut Db, sdb: &mut Db| {
            for i in 0..fields.len() {
                let sig = format!("g{}()", i);
                let g = call(gdb, ga, selector(&sig).to_vec());
                let s = call(sdb, sa, selector(&sig).to_vec());
                assert_eq!(g.success, s.success, "seed {}: {} success mismatch", seed, sig);
                assert_eq!(g.output, s.output, "seed {}: {} value diverged\ngum:\n{}", seed, sig, gum);
            }
        };

        for _ in 0..140 {
            let k = (rng.next_u64() as usize) % fields.len();
            let (g, s, arith) = fields[k];
            // set, add, sub, or mul-by-literal (the last three only for numeric
            // fields). mul takes no argument; the rest take one field-typed word.
            let choice = if arith { rng.next_u64() % 4 } else { 0 };
            let (sig, data) = match choice {
                1 => {
                    let sig = format!("a{}({})", k, s);
                    let d = encode_words(&sig, &[encode_arg(&mut rng, g)]);
                    (sig, d)
                }
                2 => {
                    let sig = format!("b{}({})", k, s);
                    let d = encode_words(&sig, &[encode_arg(&mut rng, g)]);
                    (sig, d)
                }
                3 => {
                    let sig = format!("m{}()", k);
                    let d = selector(&sig).to_vec();
                    (sig, d)
                }
                _ => {
                    let sig = format!("s{}({})", k, s);
                    let d = encode_words(&sig, &[encode_arg(&mut rng, g)]);
                    (sig, d)
                }
            };
            let g = call(&mut gdb, ga, data.clone());
            let s2 = call(&mut sdb, sa, data);
            assert_eq!(
                g.success, s2.success,
                "seed {}: {} success mismatch\ngum:\n{}",
                seed, sig, gum
            );
            if g.success {
                // The touched field must read back identically on both sides.
                let gg = call(&mut gdb, ga, selector(&format!("g{}()", k)).to_vec());
                let sg = call(&mut sdb, sa, selector(&format!("g{}()", k)).to_vec());
                assert_eq!(gg.output, sg.output, "seed {}: after {} field {} diverged\ngum:\n{}", seed, sig, k, gum);
            }
        }
        // Final full sweep: every field must still agree (catches a write to one
        // field silently corrupting a packed slot-mate).
        getters_match(&mut gdb, &mut sdb);
    }
}

// Random checked arithmetic with the literal in either operand position. For each
// numeric type this builds three twins per operator: literal-left, literal-right,
// and variable-variable. A narrow overflow must revert regardless of whether the
// literal sits left or right, so any success/value divergence from Solidity is a
// gum bug in operand-type inference or the checked-arithmetic bound. This is the
// bug class the by-hand literal-position test locked down; the fuzzer generalizes
// it across every width, sign and operator.
#[test]
fn fuzz_literal_position_arithmetic_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    // (gum type, solidity type, bit width)
    let types: &[(&str, &str, usize)] = &[
        ("u8", "uint8", 8),
        ("u16", "uint16", 16),
        ("u32", "uint32", 32),
        ("u64", "uint64", 64),
        ("u128", "uint128", 128),
        ("u256", "uint256", 256),
        ("i8", "int8", 8),
        ("i32", "int32", 32),
        ("i128", "int128", 128),
        ("i256", "int256", 256),
    ];
    let ops = ["+", "-", "*"];

    let encode_arg = |rng: &mut Rng, ty: &str, bits: usize| -> [u8; 32] {
        let signed = ty.starts_with('i');
        let raw = rng.next_u256(true);
        let val = if bits >= 256 {
            raw
        } else {
            let mask = (U256::from(1u64) << bits) - U256::from(1u64);
            let low = raw & mask;
            if signed && ((low >> (bits - 1)) & U256::from(1u64)) == U256::from(1u64) {
                low | !mask
            } else {
                low
            }
        };
        val.to_be_bytes::<32>()
    };

    for &(g, s, bits) in types {
        // A small literal that still reaches overflow when multiplied near max.
        let mut gum = String::from("contract C:\n");
        let mut sol = String::from("// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract C {\n");
        for (oi, op) in ops.iter().enumerate() {
            let c = 3;
            // literal-left, literal-right, variable-variable
            gum.push_str(&format!("    export fn ll{oi}({g} v) -> {g}:\n        return {c} {op} v\n", oi = oi, g = g, c = c, op = op));
            gum.push_str(&format!("    export fn lr{oi}({g} v) -> {g}:\n        return v {op} {c}\n", oi = oi, g = g, c = c, op = op));
            gum.push_str(&format!("    export fn vv{oi}({g} v, {g} w) -> {g}:\n        return v {op} w\n", oi = oi, g = g, op = op));
            sol.push_str(&format!("  function ll{oi}({s} v) external pure returns ({s}) {{ return {c} {op} v; }}\n", oi = oi, s = s, c = c, op = op));
            sol.push_str(&format!("  function lr{oi}({s} v) external pure returns ({s}) {{ return v {op} {c}; }}\n", oi = oi, s = s, c = c, op = op));
            sol.push_str(&format!("  function vv{oi}({s} v, {s} w) external pure returns ({s}) {{ return v {op} w; }}\n", oi = oi, s = s, op = op));
        }
        sol.push_str("}\n");

        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gum_creation_bytecode(&gum, &solc, false));
        let sa = deploy(&mut sdb, sol_creation_bytecode(&sol, &solc));

        let mut rng = Rng(0x1117_0000 ^ (bits as u64));
        for _ in 0..600 {
            let oi = (rng.next_u64() as usize) % ops.len();
            let form = rng.next_u64() % 3;
            let (sig, data) = match form {
                0 => {
                    let sig = format!("ll{}({})", oi, s);
                    (sig.clone(), encode_words(&sig, &[encode_arg(&mut rng, g, bits)]))
                }
                1 => {
                    let sig = format!("lr{}({})", oi, s);
                    (sig.clone(), encode_words(&sig, &[encode_arg(&mut rng, g, bits)]))
                }
                _ => {
                    let sig = format!("vv{}({},{})", oi, s, s);
                    (sig.clone(), encode_words(&sig, &[encode_arg(&mut rng, g, bits), encode_arg(&mut rng, g, bits)]))
                }
            };
            let gr = call(&mut gdb, ga, data.clone());
            let sr = call(&mut sdb, sa, data);
            assert_eq!(gr.success, sr.success, "{}: success mismatch\ngum:\n{}", sig, gum);
            if gr.success {
                assert_eq!(gr.output, sr.output, "{}: value diverged\ngum:\n{}", sig, gum);
            }
        }
    }
}

// Random values pushed through the dynamic ABI codecs: a (uint256,string) struct
// echo and a uint256[][] echo. The values, lengths and nesting shapes are fuzzed
// so the head/tail offset math is exercised across empty, short, word-boundary and
// multi-word payloads, diffing gum's encode+decode against Solidity's byte for byte.
#[test]
fn fuzz_dynamic_abi_roundtrip_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    // (uint256,string) struct echo.
    {
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gum_creation_bytecode(include_str!("fixtures/gum_dyn_struct.gum"), &solc, false));
        let sa = deploy(&mut sdb, sol_creation_bytecode(include_str!("fixtures/sol_dyn_struct.sol"), &solc));

        let encode = |id: U256, name: &[u8]| -> Vec<u8> {
            let mut data = selector("echo((uint256,string))").to_vec();
            data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
            data.extend_from_slice(&id.to_be_bytes::<32>());
            data.extend_from_slice(&U256::from(64u64).to_be_bytes::<32>());
            data.extend_from_slice(&U256::from(name.len()).to_be_bytes::<32>());
            let mut nb = name.to_vec();
            let pad = (32 - (name.len() % 32)) % 32;
            nb.extend(std::iter::repeat(0u8).take(pad));
            data.extend_from_slice(&nb);
            data
        };

        let mut rng = Rng(0x2229_0000);
        for _ in 0..300 {
            let id = rng.next_u256(true);
            let len = (rng.next_u64() % 130) as usize;
            let name: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xff) as u8).collect();
            let data = encode(id, &name);
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(g.success, s.success, "struct echo success (len {})", len);
            assert_eq!(g.output, s.output, "struct echo output (len {})", len);
        }
    }

    // uint256[][] echo.
    {
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gum_creation_bytecode_for(include_str!("fixtures/gum_nest_abi.gum"), &solc, false, "N"));
        let sa = deploy(&mut sdb, sol_creation_bytecode_for(include_str!("fixtures/sol_nest_abi.sol"), &solc, "N"));

        // Encode a uint256[][] as an echo argument: outer offset, outer length,
        // one offset per row (relative to the row-offset block), then each row.
        let encode = |rows: &[Vec<U256>]| -> Vec<u8> {
            let mut data = selector("echo(uint256[][])").to_vec();
            data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>()); // offset to outer array
            data.extend_from_slice(&U256::from(rows.len()).to_be_bytes::<32>());
            // Row offsets are measured from the start of the offset table.
            let mut off = 32u64 * rows.len() as u64;
            let mut tail: Vec<u8> = Vec::new();
            for row in rows {
                data.extend_from_slice(&U256::from(off).to_be_bytes::<32>());
                tail.extend_from_slice(&U256::from(row.len()).to_be_bytes::<32>());
                for v in row {
                    tail.extend_from_slice(&v.to_be_bytes::<32>());
                }
                off += 32 + 32 * row.len() as u64;
            }
            data.extend_from_slice(&tail);
            data
        };

        let mut rng = Rng(0x333a_0000);
        for _ in 0..300 {
            let nrows = (rng.next_u64() % 5) as usize;
            let rows: Vec<Vec<U256>> = (0..nrows)
                .map(|_| {
                    let cols = (rng.next_u64() % 5) as usize;
                    (0..cols).map(|_| rng.next_u256(true)).collect()
                })
                .collect();
            let data = encode(&rows);
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(g.success, s.success, "nested echo success ({} rows)", nrows);
            assert_eq!(g.output, s.output, "nested echo output ({} rows)", nrows);
        }
    }
}

// Number of accumulator variables the control-flow fuzzer works with. A try body
// that mutates several of them writes them all back as one ABI tuple, so keeping
// this above one exercises the multi-field write-back path.
const CTRL_VARS: usize = 3;

// One generated control-flow statement. The generator emits these; the same tree
// is rendered to gum source and evaluated by a reference oracle, so the two can
// never drift. Add mutates one variable, Assert reverts when it fails, Ret returns
// the running total, and Try wraps a body plus a catch handler.
#[derive(Clone)]
enum CtrlStmt {
    Add(usize, u64),
    Assert(u64),
    Ret,
    Try(Vec<CtrlStmt>, Vec<CtrlStmt>),
}

// A random non-empty block, nested up to `depth` levels of try/catch. A try body
// may end in a return (the propagate-out path); a catch never does, so code after
// a try stays reachable through the caught path and gum's return analysis holds.
fn gen_ctrl_block(rng: &mut Rng, depth: u32) -> Vec<CtrlStmt> {
    let n = 1 + (rng.next_u64() % 3) as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let pick = rng.next_u64() % 100;
        if depth > 0 && pick < 35 {
            let mut b = gen_ctrl_block(rng, depth - 1);
            if rng.next_u64() % 3 == 0 {
                b.push(CtrlStmt::Ret);
            }
            let h = gen_ctrl_block(rng, depth - 1);
            v.push(CtrlStmt::Try(b, h));
        } else if pick < 67 {
            v.push(CtrlStmt::Assert(1 + rng.next_u64() % 24));
        } else {
            let vi = (rng.next_u64() as usize) % CTRL_VARS;
            v.push(CtrlStmt::Add(vi, 1 + rng.next_u64() % 4));
        }
    }
    v
}

// Render a block to gum source at the given indent level.
fn render_ctrl(stmts: &[CtrlStmt], level: usize, out: &mut String) {
    let ind = "    ".repeat(level);
    let sum = (0..CTRL_VARS).map(|i| format!("r{}", i)).collect::<Vec<_>>().join(" + ");
    for s in stmts {
        match s {
            CtrlStmt::Add(vi, c) => out.push_str(&format!("{ind}r{vi} = r{vi} + {c}\n")),
            CtrlStmt::Assert(k) => out.push_str(&format!("{ind}assert(a < {k}, \"m\")\n")),
            CtrlStmt::Ret => out.push_str(&format!("{ind}return {sum}\n")),
            CtrlStmt::Try(b, h) => {
                out.push_str(&format!("{ind}try:\n"));
                render_ctrl(b, level + 1, out);
                out.push_str(&format!("{ind}catch:\n"));
                render_ctrl(h, level + 1, out);
            }
        }
    }
}

// The reference oracle outcome for a block: fell through, returned a value out of
// the function, or reverted.
enum CtrlFlow {
    Fell,
    Returned(U256),
    Reverted,
}

// A try runs its body; on revert the catch runs with the pre-try variables
// restored, exactly as an EVM self-call frame rolls back its locals. A return
// inside a try propagates out of the whole function.
fn eval_ctrl(stmts: &[CtrlStmt], a: U256, vars: &mut [U256; CTRL_VARS]) -> CtrlFlow {
    for s in stmts {
        match s {
            CtrlStmt::Add(vi, c) => vars[*vi] += U256::from(*c),
            CtrlStmt::Assert(k) => {
                if a >= U256::from(*k) {
                    return CtrlFlow::Reverted;
                }
            }
            CtrlStmt::Ret => return CtrlFlow::Returned(vars.iter().copied().fold(U256::ZERO, |x, y| x + y)),
            CtrlStmt::Try(b, h) => {
                let snapshot = *vars;
                match eval_ctrl(b, a, vars) {
                    CtrlFlow::Fell => {}
                    CtrlFlow::Returned(v) => return CtrlFlow::Returned(v),
                    CtrlFlow::Reverted => {
                        *vars = snapshot;
                        match eval_ctrl(h, a, vars) {
                            CtrlFlow::Fell => {}
                            CtrlFlow::Returned(v) => return CtrlFlow::Returned(v),
                            CtrlFlow::Reverted => return CtrlFlow::Reverted,
                        }
                    }
                }
            }
        }
    }
    CtrlFlow::Fell
}

// Random nested try/catch diffed against a reference oracle. gum has no Solidity
// twin for internal try/catch (Solidity only guards external calls), so the
// oracle is the reference. Each try becomes a hoisted self-call frame: this
// exercises capturing the accumulator in, writing it back on success, and rolling
// it back on a caught revert, plus revert propagation to the right catch level.
#[test]
fn fuzz_try_catch_control_flow_matches_reference() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    for prog in 0..80u64 {
        let mut rng = Rng(0x7c0f_0000 ^ prog);
        let body = gen_ctrl_block(&mut rng, 3);
        // The function declares CTRL_VARS accumulators, all seeded to a, and
        // returns their sum, so a try that mutates several of them must write
        // every one back for the total to match.
        let decls: String = (0..CTRL_VARS).map(|i| format!("        mut u256 r{} = a\n", i)).collect();
        let sum = (0..CTRL_VARS).map(|i| format!("r{}", i)).collect::<Vec<_>>().join(" + ");
        // Half the programs put the body in a non-entry helper reached by an
        // internal call, so the non-entry return/write-back lowering is covered
        // too; the call is transparent, so the oracle is identical either way.
        let via_helper = rng.next_u64() % 2 == 0;
        let mut src = String::from("contract C:\n");
        if via_helper {
            src.push_str(&format!("    fn g(u256 a) -> u256:\n{}", decls));
            render_ctrl(&body, 2, &mut src);
            src.push_str(&format!("        return {sum}\n\n    export fn f(u256 a) -> u256:\n        return C.g(a)\n"));
        } else {
            src.push_str(&format!("    export fn f(u256 a) -> u256:\n{}", decls));
            render_ctrl(&body, 2, &mut src);
            src.push_str(&format!("        return {sum}\n"));
        }

        let mut db: Db = CacheDB::new(EmptyDB::default());
        let c = deploy(&mut db, gum_creation_bytecode(&src, &solc, false));

        for _ in 0..16 {
            let a = U256::from(rng.next_u64() % 25);
            let mut d = selector("f(uint256)").to_vec();
            d.extend_from_slice(&a.to_be_bytes::<32>());
            let r = call(&mut db, c, d);
            let mut vars = [a; CTRL_VARS];
            let expected = match eval_ctrl(&body, a, &mut vars) {
                CtrlFlow::Fell => vars.iter().copied().fold(U256::ZERO, |x, y| x + y),
                CtrlFlow::Returned(v) => v,
                CtrlFlow::Reverted => {
                    assert!(!r.success, "prog {}: a={} expected revert got success\nsrc:\n{}", prog, a, src);
                    continue;
                }
            };
            assert!(r.success, "prog {}: a={} expected {} got revert\nsrc:\n{}", prog, a, expected, src);
            assert_eq!(U256::from_be_slice(&r.output), expected, "prog {}: a={} value diverged\nsrc:\n{}", prog, a, src);
        }
    }
}

// A try body that both mutates a captured local and returns, inside a non-entry
// helper called internally. Regression for a silent try-downgrade the control-flow
// fuzzer surfaced: this shape could not be hoisted, so it stayed on the inline path
// that cannot catch an internal revert, and the assert reverted the whole call
// instead of reaching the catch. Both the return path and the caught path must work.
#[test]
fn try_that_returns_and_writes_back_is_caught_through_an_internal_call() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/gum_try_internal_return.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let call_f = |db: &mut Db, a: u64| -> (bool, U256) {
        let mut d = selector("f(uint256)").to_vec();
        d.extend_from_slice(&U256::from(a).to_be_bytes::<32>());
        let r = call(db, c, d);
        (r.success, U256::from_be_slice(&r.output))
    };
    // a < 2: assert holds, the try returns r = a + 4 from inside the frame.
    assert_eq!(call_f(&mut db, 0), (true, U256::from(4u64)));
    assert_eq!(call_f(&mut db, 1), (true, U256::from(5u64)));
    // a >= 2: the assert reverts inside the try, the frame rolls r back to a, the
    // catch runs (r = a + 3) and the helper returns it. A revert here would mean
    // the catch was silently skipped.
    assert_eq!(call_f(&mut db, 5), (true, U256::from(8u64)));
    assert_eq!(call_f(&mut db, 20), (true, U256::from(23u64)));
}

// A try body that mutates two variables of different types (a u256 and a u8) and
// returns a value of a third type (String). Exercises the multi-field, mixed-type
// write-back tuple plus a differently-typed return, all in one try: on the caught
// path both variables roll back and the write-back tuple carries their post-catch
// values, on the return path the String propagates out.
#[test]
fn try_writes_back_multiple_mixed_type_variables_and_returns_another_type() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/gum_try_multi_writeback.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let call_f = |db: &mut Db, a: u64| -> (bool, String) {
        let mut d = selector("f(uint256)").to_vec();
        d.extend_from_slice(&U256::from(a).to_be_bytes::<32>());
        let r = call(db, c, d);
        // Decode an ABI string return: [offset][len][bytes].
        let len = U256::from_be_slice(&r.output[32..64]).to::<usize>();
        (r.success, String::from_utf8(r.output[64..64 + len].to_vec()).unwrap())
    };
    // a < 3: assert holds, the try returns the String directly.
    assert_eq!(call_f(&mut db, 0), (true, "returned".to_string()));
    // a >= 3: the assert reverts inside the try, count rolls back to a, the catch
    // runs (count = a + 100) and the write-back carries it out; f returns its text.
    assert_eq!(call_f(&mut db, 5), (true, "105".to_string()));
    assert_eq!(call_f(&mut db, 42), (true, "142".to_string()));
}

// Narrow signed integers in every container must round-trip through a read +
// checked-arithmetic exactly as Solidity: a negative iN is read sign-extended, so
// its underflow is caught, not silently wrapped. A regression for the bug the
// structural fuzzer surfaced (found first in a plain field; the same read path
// underlies mapping values, array elements and struct fields).
#[test]
fn narrow_signed_underflow_reverts_in_every_container() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let neg = |v: i32| {
        let mut w = [0u8; 32];
        if v < 0 {
            for b in w[..28].iter_mut() {
                *b = 0xff;
            }
        }
        w[28..32].copy_from_slice(&v.to_be_bytes());
        w
    };
    let gum = include_str!("fixtures/gum_signed_containers.gum");
    let sol = include_str!("fixtures/sol_signed_containers.sol");
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum_creation_bytecode(gum, &solc, false));
    let sa = deploy(&mut sdb, sol_creation_bytecode(sol, &solc));
    let step = |gdb: &mut Db, sdb: &mut Db, sig: &str, args: &[[u8; 32]]| -> (bool, bool) {
        let g = call(gdb, ga, encode_words(sig, args));
        let s = call(sdb, sa, encode_words(sig, args));
        (g.success, s.success)
    };
    let k = word_u256(U256::from(1u64));
    for (label, setsig, setargs, addsig, addargs) in [
        ("mapping", "setm(uint256,int32)", vec![k, neg(-2_000_000_000)], "addm(uint256,int32)", vec![k, neg(-500_000_000)]),
        ("array", "pusha(int32)", vec![neg(-2_000_000_000)], "adda(uint256,int32)", vec![word_u256(U256::ZERO), neg(-500_000_000)]),
        ("struct", "setp(int32)", vec![neg(-2_000_000_000)], "addp(int32)", vec![neg(-500_000_000)]),
    ] {
        let (gs, ss) = step(&mut gdb, &mut sdb, setsig, &setargs);
        assert!(gs && ss, "{}: set should succeed", label);
        let (ga_ok, sa_ok) = step(&mut gdb, &mut sdb, addsig, &addargs);
        assert_eq!(ga_ok, sa_ok, "{}: underflow revert must agree with Solidity", label);
        assert!(!ga_ok, "{}: underflow must revert", label);
    }
}

#[test]
fn const_fields_are_baked_in_per_deployment() {
    // Two deployments of the same creation code with different constructor
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "contract Cfg:\n    const Account owner\n    const u256 cap\n    u256 counter\n\n    \
               fn new(Account o, u256 c):\n        Cfg.owner = o\n        Cfg.cap = c\n\n    \
               export fn get_owner() -> Account:\n        return Cfg.owner\n\n    \
               export fn get_cap() -> u256:\n        return Cfg.cap\n\n    \
               export fn bump() -> u256:\n        Cfg.counter = Cfg.counter + 1\n        return Cfg.counter\n";
    let base = gum_creation_bytecode(src, &solc, false);

    let cases = [
        (Address::from([0x11u8; 20]), 100u64),
        (Address::from([0x22u8; 20]), 999u64),
    ];
    for (owner, cap) in cases {
        let mut code = base.clone();
        code.extend_from_slice(&word_addr(owner));
        code.extend_from_slice(&word_u256(U256::from(cap)));

        let mut db: Db = CacheDB::new(EmptyDB::default());
        let (addr, _) = deploy_with_gas(&mut db, code);

        let r = call(&mut db, addr, encode("get_owner()", &[]));
        assert!(r.success, "get_owner() reverted");
        assert_eq!(Address::from_slice(&r.output[12..32]), owner, "wrong const owner");

        let r = call(&mut db, addr, encode("get_cap()", &[]));
        assert!(r.success, "get_cap() reverted");
        assert_eq!(U256::from_be_slice(&r.output), U256::from(cap), "wrong const cap");

        // The storage field still behaves, i.e. the immutables took no slot.
        let r = call(&mut db, addr, encode("bump()", &[]));
        assert!(r.success, "bump() reverted");
        assert_eq!(U256::from_be_slice(&r.output), U256::from(1u64), "counter should be 1");
        let r = call(&mut db, addr, encode("bump()", &[]));
        assert_eq!(U256::from_be_slice(&r.output), U256::from(2u64), "counter should be 2");

        // Immutables are unaffected by anything the runtime does.
        let r = call(&mut db, addr, encode("get_cap()", &[]));
        assert_eq!(U256::from_be_slice(&r.output), U256::from(cap), "const field changed after a write");
    }
}

// super.foo() must reach the body foo overrode, not recurse into the
// override. Checked by value: the parent contributes 10, the child adds 5, so
// only a real parent call yields 15. Infinite recursion would run out of gas
// and a dropped parent body would yield 5.
#[test]
fn super_calls_the_overridden_method() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "class Base:\n    u256 v\n\n    fn label() -> u256:\n        return 10\n\n\
               [Base]\nclass Child:\n    fn label() -> u256:\n        return super.label() + 5\n\n\
               contract C:\n    u256 out\n\n    export fn run() -> u256:\n        \
               mut Child c = new Child()\n        return c.label()\n";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let r = call(&mut db, addr, encode("run()", &[]));
    assert!(r.success, "run() reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(15u64), "super.label() should give 10 + 5");
}

// A whole dynamic storage array read as a value copies out to memory.
// Checked by value across both a word wide and a packed element type, because storage packs 32 u8s to a slot while memory lays them out at one byte each, so a copy that ignored the repacking would still read consistently and still be wrong.
#[test]
fn a_storage_array_copies_into_memory() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(GUM_ARR_COPY, &solc, true));
    assert!(call(&mut db, addr, encode("fill()", &[])).success, "fill reverted");

    let n = call(&mut db, addr, encode("copy_len()", &[]));
    assert_eq!(U256::from_be_slice(&n.output), U256::from(3u64), "copied length");

    let s = call(&mut db, addr, encode("copy_sum()", &[]));
    assert_eq!(U256::from_be_slice(&s.output), U256::from(24u64), "7 + 8 + 9");

    for (i, want) in [(0u64, 7u64), (1, 8), (2, 9)] {
        let r = call(&mut db, addr, encode("copy_at(uint256)", &[U256::from(i)]));
        assert_eq!(U256::from_be_slice(&r.output), U256::from(want), "copied element {}", i);
    }

    let p = call(&mut db, addr, encode("copy_small_sum()", &[]));
    assert_eq!(U256::from_be_slice(&p.output), U256::from(10u64), "1 + 2 + 3 + 4 of a packed u8 array");

    let oob = call(&mut db, addr, encode("copy_at(uint256)", &[U256::from(3u64)]));
    assert!(!oob.success, "index 3 of a 3 element copy must revert");
}

// A struct crossing the ABI is the case where memory and the wire disagree about everything.
// P's fields are declared a(u128) b(u256) c(u8) d(address) e(bool), but memory sorts them widest-first, so b and d sit before a: declaration order and memory order are deliberately different here.
// That makes any copy that treats the tuple as a block, rather than moving each field, come back transposed instead of merely shifted.
#[test]
fn a_struct_crosses_the_abi_like_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_STRUCT_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_STRUCT_ABI, &solc));

    let who: Address = "0x00000000000000000000000000000000cafebabe".parse().unwrap();
    let mut bool_word = [0u8; 32];
    bool_word[31] = 1;
    let arg = [
        word_u256(U256::from(11u64)),
        word_u256(U256::from(22u64)),
        word_u256(U256::from(33u64)),
        word_addr(who),
        bool_word,
    ];

    let tuple = "(uint128,uint256,uint8,address,bool)";
    for f in ["fa", "fb", "fc", "fd", "fe", "echo"] {
        let sig = format!("{}({})", f, tuple);
        let gr = call(&mut gdb, g, encode_words(&sig, &arg));
        let sr = call(&mut sdb, s, encode_words(&sig, &arg));
        assert!(sr.success, "solidity {} reverted", f);
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} return data differs from solidity", f);
    }

    // Pinned by value too, so a shared misdecode in both compilers could not pass as agreement.
    let fa = call(&mut gdb, g, encode_words(&format!("fa({})", tuple), &arg));
    assert_eq!(U256::from_be_slice(&fa.output), U256::from(11u64), "field a");
    let fd = call(&mut gdb, g, encode_words(&format!("fd({})", tuple), &arg));
    assert_eq!(U256::from_be_slice(&fd.output), U256::from_be_slice(who.as_slice()), "field d");

    // A tuple is inline in the head, so a caller one word short must revert rather than read a zero.
    let short = encode_words(&format!("fa({})", tuple), &arg[..4]);
    assert!(!call(&mut gdb, g, short).success, "a truncated tuple must revert");

    // A struct between two scalars: the only thing that pins the cursor advancing by the tuple's whole width rather than the one word an offset would take.
    // If the advance were wrong, y would be read from inside the tuple and the sum would still look plausible.
    let mut mixed = vec![word_u256(U256::from(5u64))];
    mixed.extend_from_slice(&arg);
    mixed.push(word_u256(U256::from(7u64)));
    let msig = format!("mix(uint256,{},uint256)", tuple);
    let gm = call(&mut gdb, g, encode_words(&msig, &mixed));
    let sm = call(&mut sdb, s, encode_words(&msig, &mixed));
    assert!(gm.success && sm.success, "mix reverted");
    assert_eq!(gm.output, sm.output, "mix return data differs from solidity");
    assert_eq!(U256::from_be_slice(&gm.output), U256::from(34u64), "5 + 22 + 7");
}

// A constructor's struct arg travels a different path from a dispatcher's: it arrives appended to the creation code and is codecopy'd in, so it decodes from memory rather than calldata.
#[test]
fn a_struct_constructor_arg_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let who: Address = "0x000000000000000000000000000000000badf00d".parse().unwrap();
    let mut args = Vec::new();
    args.extend_from_slice(&word_u256(U256::from(41u64)));
    args.extend_from_slice(&word_u256(U256::from(42u64)));
    args.extend_from_slice(&word_addr(who));

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let mut gc = gum_creation_bytecode(GUM_STRUCT_CTOR, &solc, false);
    gc.extend_from_slice(&args);
    let g = deploy(&mut gdb, gc);
    let mut sc = sol_creation_bytecode(SOL_STRUCT_CTOR, &solc);
    sc.extend_from_slice(&args);
    let s = deploy(&mut sdb, sc);

    for (f, want) in [("get_a()", U256::from(41u64)), ("get_b()", U256::from(42u64))] {
        let gr = call(&mut gdb, g, encode(f, &[]));
        let sr = call(&mut sdb, s, encode(f, &[]));
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
        assert_eq!(U256::from_be_slice(&gr.output), want, "{}", f);
    }
    let gd = call(&mut gdb, g, encode("get_d()", &[]));
    let sd = call(&mut sdb, s, encode("get_d()", &[]));
    assert_eq!(gd.output, sd.output, "get_d differs from solidity");
    assert_eq!(U256::from_be_slice(&gd.output), U256::from_be_slice(who.as_slice()), "address field");
}

// A struct passed to new Child(...) goes through the CREATE arg encoder, a third path that neither the dispatcher nor the constructor decode touches.
#[test]
fn a_struct_passed_to_new_contract_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy_named(&mut gdb, &solc, GUM_STRUCT_DEPLOY, "Parent");
    let s = deploy(&mut sdb, sol_creation_bytecode_for(SOL_STRUCT_DEPLOY, &solc, "Parent"));

    let arg = [word_u256(U256::from(3u64)), word_u256(U256::from(77u64))];
    let sig = "make_and_read((uint128,uint256))";
    let gr = call(&mut gdb, g, encode_words(sig, &arg));
    let sr = call(&mut sdb, s, encode_words(sig, &arg));
    assert!(sr.success, "solidity make_and_read reverted");
    assert!(gr.success, "gum make_and_read reverted");
    assert_eq!(gr.output, sr.output, "make_and_read differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(77u64), "child read field b");
}

// gum calling Solidity over an interface, which is the strongest available check on the arg encoder: solc's decoder is the reference, and it reverts on a malformed head or tail rather than guessing.
#[test]
fn interface_calls_encode_non_scalar_args_for_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let sink = deploy(&mut db, sol_creation_bytecode_for(SOL_IFACE_SINK, &solc, "Sink"));
    let caller = deploy_named(&mut db, &solc, GUM_IFACE_CALL, "Caller");

    let r = call(
        &mut db,
        caller,
        encode_words(
            "fwd(address,(uint128,uint256))",
            &[word_addr(sink), word_u256(U256::from(4u64)), word_u256(U256::from(38u64))],
        ),
    );
    assert!(r.success, "fwd reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(42u64), "solidity read the tuple as 4 + 38");

    let mut sd = selector("fwd_str(address,string)").to_vec();
    sd.extend_from_slice(&word_addr(sink));
    sd.extend_from_slice(&word_u256(U256::from(64u64)));
    sd.extend_from_slice(&word_u256(U256::from(5u64)));
    let mut w = [0u8; 32];
    w[..5].copy_from_slice(b"hello");
    sd.extend_from_slice(&w);
    let r = call(&mut db, caller, sd);
    assert!(r.success, "fwd_str reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(5u64), "solidity read the string length");

    let mut ad = selector("fwd_arr(address,uint256[])").to_vec();
    ad.extend_from_slice(&word_addr(sink));
    ad.extend_from_slice(&word_u256(U256::from(64u64)));
    ad.extend_from_slice(&word_u256(U256::from(3u64)));
    for v in [10u64, 20, 30] {
        ad.extend_from_slice(&word_u256(U256::from(v)));
    }
    let r = call(&mut db, caller, ad);
    assert!(r.success, "fwd_arr reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(60u64), "solidity summed the array");

    // A struct array through an interface, the one arg shape that is dynamic and has a per-element codec: an offset word, a count, then inline tuples.
    let mut td = selector("fwd_starr(address,(uint128,uint256)[])").to_vec();
    td.extend_from_slice(&word_addr(sink));
    td.extend_from_slice(&word_u256(U256::from(64u64)));
    td.extend_from_slice(&word_u256(U256::from(2u64)));
    for (a, b) in [(1u64, 10u64), (2, 20)] {
        td.extend_from_slice(&word_u256(U256::from(a)));
        td.extend_from_slice(&word_u256(U256::from(b)));
    }
    let r = call(&mut db, caller, td);
    assert!(r.success, "fwd_starr reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(33u64), "solidity summed 1+10+2+20 over the tuple array");

    let r = call(
        &mut db,
        caller,
        encode_words("fwd_mk(address,uint256)", &[word_addr(sink), word_u256(U256::from(99u64))]),
    );
    assert!(r.success, "fwd_mk reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(99u64), "gum decoded field b of solidity's returned tuple");
    // Non-scalar returns: a string, a scalar array, and a struct array coming back out of returndata.
    let r = call(&mut db, caller, encode_words("fwd_name_len(address)", &[word_addr(sink)]));
    assert!(r.success, "fwd_name_len reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(7u64), "length of \"gumball\", not the offset word");

    let r = call(&mut db, caller, encode_words("fwd_name(address)", &[word_addr(sink)]));
    assert!(r.success, "fwd_name reverted");
    // Re-encoded by gum on the way out, so this is the full round trip: solidity -> gum memory -> the wire.
    assert_eq!(&r.output[64..71], b"gumball", "the string itself survived the round trip");
    assert_eq!(U256::from_be_slice(&r.output[32..64]), U256::from(7u64), "returned length word");

    let r = call(&mut db, caller, encode_words("fwd_nums_sum(address)", &[word_addr(sink)]));
    assert!(r.success, "fwd_nums_sum reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(18u64), "5 + 6 + 7 from a returned uint256[]");

    for (i, want) in [(0u64, 111u64), (1, 222)] {
        let r = call(&mut db, caller, encode_words("fwd_pairs_b(address,uint256)", &[word_addr(sink), word_u256(U256::from(i))]));
        assert!(r.success, "fwd_pairs_b reverted");
        assert_eq!(U256::from_be_slice(&r.output), U256::from(want), "field b of returned tuple array element {}", i);
    }

}

// An array of static structs: elements are inline on the wire at the tuple's full width, but inline and packed in memory at a different width, and in a different field order.
#[test]
fn a_struct_array_crosses_the_abi_like_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_STARR_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_STARR_ABI, &solc));

    let who: [Address; 2] = [
        "0x00000000000000000000000000000000cafebabe".parse().unwrap(),
        "0x000000000000000000000000000000000badf00d".parse().unwrap(),
    ];
    // Two elements: (1, 100, cafebabe) and (2, 200, badf00d).
    let mut arr = Vec::new();
    arr.extend_from_slice(&word_u256(U256::from(2u64)));
    for (i, a) in [1u64, 2].iter().enumerate() {
        arr.extend_from_slice(&word_u256(U256::from(*a)));
        arr.extend_from_slice(&word_u256(U256::from((i as u64 + 1) * 100)));
        arr.extend_from_slice(&word_addr(who[i]));
    }

    let tup = "(uint128,uint256,address)[]";
    for f in ["count", "sum_b", "echo"] {
        let sig = format!("{}({})", f, tup);
        let mut d = selector(&sig).to_vec();
        d.extend_from_slice(&word_u256(U256::from(32u64)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(sr.success, "solidity {} reverted", f);
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
    }

    // Indexed access, where the array sits behind two head words so its offset is 64.
    for (f, i, want) in [("at_a", 0u64, U256::from(1u64)), ("at_a", 1, U256::from(2u64))] {
        let sig = format!("{}({},uint256)", f, tup);
        let mut d = selector(&sig).to_vec();
        d.extend_from_slice(&word_u256(U256::from(64u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "{}[{}] differs from solidity", f, i);
        assert_eq!(U256::from_be_slice(&gr.output), want, "{}[{}] by value", f, i);
    }

    // Assigning a whole element copies its bytes. mstore instead wrote the source element's address into field a and left b and c alone, so reading b back still looked right.
    {
        let mut d = selector(&format!("copy_elem({},uint256,uint256)", tup)).to_vec();
        d.extend_from_slice(&word_u256(U256::from(96u64)));
        d.extend_from_slice(&word_u256(U256::from(0u64)));
        d.extend_from_slice(&word_u256(U256::from(1u64)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(gr.success, "gum copy_elem reverted");
        assert_eq!(gr.output, sr.output, "copy_elem differs from solidity");
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(200u64), "element 1's b after copying it over element 0");
    }

    let mut d = selector(&format!("at_c({},uint256)", tup)).to_vec();
    d.extend_from_slice(&word_u256(U256::from(64u64)));
    d.extend_from_slice(&word_u256(U256::from(1u64)));
    d.extend_from_slice(&arr);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "at_c differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from_be_slice(who[1].as_slice()), "address field of element 1");

    // Writing through a memory struct array, which is what the element-address path exists for.
    let mut d = selector(&format!("bump({},uint256,uint128)", tup)).to_vec();
    d.extend_from_slice(&word_u256(U256::from(96u64)));
    d.extend_from_slice(&word_u256(U256::from(1u64)));
    d.extend_from_slice(&word_u256(U256::from(9u64)));
    d.extend_from_slice(&arr);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum bump reverted");
    assert_eq!(gr.output, sr.output, "bump differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(9u64), "wrote and read back element 1 field a");

    // Out of bounds must revert rather than read past the array.
    let mut d = selector(&format!("at_a({},uint256)", tup)).to_vec();
    d.extend_from_slice(&word_u256(U256::from(64u64)));
    d.extend_from_slice(&word_u256(U256::from(2u64)));
    d.extend_from_slice(&arr);
    assert!(!call(&mut gdb, g, d).success, "index 2 of a 2 element array must revert");
}

// Message and Block are the ambient EVM state, so each accessor is only correct if it reaches the right opcode.
#[test]
fn message_and_block_read_the_same_state_as_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_MSG_BLOCK, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_MSG_BLOCK, &solc));

    // Frame-scoped: the immediate caller, not whoever signed.
    let caller: Address = "0x00000000000000000000000000000000deadbeef".parse().unwrap();
    let gr = call_from(&mut gdb, caller, g, encode("who()", &[]));
    let sr = call_from(&mut sdb, caller, s, encode("who()", &[]));
    assert_eq!(gr.output, sr.output, "who() differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from_be_slice(caller.as_slice()), "sender is the caller");

    let gr = call_with_value(&mut gdb, caller, g, encode("amount()", &[]), U256::from(1234u64));
    assert!(gr.success, "amount() reverted");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(1234u64), "value is the attached wei");

    // address() is each contract's own, which differs per deployment, so it is checked against each rather than diffed.
    let gr = call(&mut gdb, g, encode("me()", &[]));
    assert_eq!(U256::from_be_slice(&gr.output), U256::from_be_slice(g.as_slice()), "gum me() is its own address");
    let sr = call(&mut sdb, s, encode("me()", &[]));
    assert_eq!(U256::from_be_slice(&sr.output), U256::from_be_slice(s.as_slice()), "sol me() is its own address");

    // Block-scoped: identical in both, since both run under the same block env.
    for f in ["when()", "height()"] {
        let gr = call(&mut gdb, g, encode(f, &[]));
        let sr = call(&mut sdb, s, encode(f, &[]));
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
    }
}

// An enum is one uint8 word on the wire but a pointer to [tag][payload] in memory, so every boundary converts.
#[test]
fn enums_cross_the_abi_like_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_ENUM_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_ENUM_ABI, &solc));

    // An argument after an enum. This is the one that returned 0.
    for tag in [0u64, 1, 2] {
        let d = encode("after_enum(uint8,uint256)", &[U256::from(tag), U256::from(42u64)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(gr.success, "after_enum reverted");
        assert_eq!(gr.output, sr.output, "after_enum({}) differs from solidity", tag);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(42u64), "the argument after an enum survives");
    }

    // An enum between two arguments: both neighbours have to land.
    let d = encode("between(uint256,uint8,uint256)", &[U256::from(7u64), U256::from(1u64), U256::from(9u64)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "between differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(16u64), "7 + 9 across an enum");

    // The tag itself still reaches match.
    for (tag, want) in [(0u64, 10u64), (1, 20), (2, 30)] {
        let d = encode("tag(uint8)", &[U256::from(tag)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "tag({}) differs from solidity", tag);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(want), "match on tag {}", tag);
    }

    // Returning an enum: the tag, not a memory pointer.
    for tag in [0u64, 1, 2] {
        let d = encode("echo(uint8)", &[U256::from(tag)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "echo({}) differs from solidity", tag);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(tag), "echo returns the tag");
    }

    for (x, want) in [(0u64, 0u64), (5, 2)] {
        let d = encode("pick(uint256)", &[U256::from(x)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "pick({}) differs from solidity", x);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(want), "pick returns a tag");
    }
    // An array of enums. A payload-free enum is a u8, so [S] is a uint8[]: one byte per element, 32 to a word, exactly like a [u8].
    let mut d = selector("count_closed(uint8[])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&word_u256(U256::from(5u64)));
    for tag in [0u64, 2, 2, 1, 2] {
        d.extend_from_slice(&word_u256(U256::from(tag)));
    }
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum count_closed reverted");
    assert!(sr.success, "sol count_closed reverted");
    assert_eq!(gr.output, sr.output, "count_closed differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(3u64), "three Closed in the array");

}

// A function the ABI calls view has to survive STATICCALL, which is what solc emits when one contract calls another's view function. Anything that writes state reverts in there.
#[test]
fn view_functions_survive_a_staticcall() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let v = deploy(&mut db, gum_creation_bytecode(GUM_VIEW, &solc, true));
    let prober = deploy(&mut db, sol_creation_bytecode_for(SOL_PROBER, &solc, "Prober"));

    let probe = |db: &mut Db, inner: Vec<u8>| -> bool {
        // probe(address,bytes): the target, then a dynamic bytes tail.
        let mut d = selector("probe(address,bytes)").to_vec();
        d.extend_from_slice(&word_addr(v));
        d.extend_from_slice(&word_u256(U256::from(64u64)));
        d.extend_from_slice(&word_u256(U256::from(inner.len() as u64)));
        let mut padded = inner.clone();
        while padded.len() % 32 != 0 {
            padded.push(0);
        }
        d.extend_from_slice(&padded);
        let r = call(db, prober, d);
        assert!(r.success, "the prober itself reverted");
        U256::from_be_slice(&r.output) == U256::from(1u64)
    };

    // Read-only: these are the ones now advertised as view/pure.
    assert!(probe(&mut db, encode("get_total()", &[])), "get_total must survive STATICCALL");
    assert!(
        probe(&mut db, encode_words("balance_of(address)", &[word_addr(deployer())])),
        "balance_of must survive STATICCALL"
    );
    assert!(probe(&mut db, encode("double(uint256)", &[U256::from(21u64)])), "double must survive STATICCALL");
    assert!(probe(&mut db, encode("sender()", &[])), "sender must survive STATICCALL");

    // A writer must still fail under STATICCALL: the ABI calls it nonpayable, and this is what that means.
    assert!(
        !probe(&mut db, encode("set_total(uint256)", &[U256::from(5u64)])),
        "set_total writes storage, so STATICCALL must reject it"
    );

    // And the read-only ones still return the right answers on a normal call.
    assert!(call(&mut db, v, encode("set_total(uint256)", &[U256::from(77u64)])).success);
    let r = call(&mut db, v, encode("get_total()", &[]));
    assert_eq!(U256::from_be_slice(&r.output), U256::from(77u64), "get_total still reads storage");
    let r = call(&mut db, v, encode("double(uint256)", &[U256::from(21u64)]));
    assert_eq!(U256::from_be_slice(&r.output), U256::from(42u64), "double still computes");
}


// An enum in storage, in a mapping, and in a log. All three used to write the enum's memory pointer rather than its value, because size_of(enum) claimed 64 bytes for a [tag][payload] pair.
#[test]
fn enum_state_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_ENUM_STATE, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_ENUM_STATE, &solc));

    // Storage: set, read back, and check the enum really is one packed byte.
    for tag in [0u64, 1, 2] {
        let d = encode("set_state(uint8)", &[U256::from(tag)]);
        assert!(call(&mut gdb, g, d.clone()).success, "gum set_state reverted");
        assert!(call(&mut sdb, s, d).success, "sol set_state reverted");
        let gr = call(&mut gdb, g, encode("get_state()", &[]));
        let sr = call(&mut sdb, s, encode("get_state()", &[]));
        assert_eq!(gr.output, sr.output, "get_state({}) differs from solidity", tag);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(tag), "the tag survives a storage round trip");
    }

    // The enum must occupy one byte, not a whole slot and certainly not two.
    assert!(call(&mut gdb, g, encode("set_after(uint256)", &[U256::from(12345u64)])).success);
    let gr = call(&mut gdb, g, encode("get_after()", &[]));
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(12345u64), "the field after an enum is not clobbered");
    let gr = call(&mut gdb, g, encode("get_state()", &[]));
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(2u64), "and the enum still reads back after its neighbour was written");

    // Mapping value: this used to sstore the memory pointer.
    let who: Address = "0x00000000000000000000000000000000cafebabe".parse().unwrap();
    for tag in [0u64, 2, 1] {
        let d = encode_words("set_user(address,uint8)", &[word_addr(who), word_u256(U256::from(tag))]);
        assert!(call(&mut gdb, g, d.clone()).success, "gum set_user reverted");
        assert!(call(&mut sdb, s, d).success, "sol set_user reverted");
        let gr = call(&mut gdb, g, encode_words("get_user(address)", &[word_addr(who)]));
        let sr = call(&mut sdb, s, encode_words("get_user(address)", &[word_addr(who)]));
        assert_eq!(gr.output, sr.output, "get_user({}) differs from solidity", tag);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(tag), "the tag survives a mapping round trip");
    }

    // Log: the data word used to be the pointer, and topic0 was hashed from "Changed(uint256)" while the parameter path called the same enum uint8.
    for tag in [0u64, 1, 2] {
        let d = encode("emit_it(uint8)", &[U256::from(tag)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(gr.success, "gum emit_it reverted");
        assert_eq!(gr.logs.len(), 1, "one log");
        assert_eq!(sr.logs.len(), 1, "one log");
        assert_eq!(gr.logs[0].0, sr.logs[0].0, "topic0 differs: the event signature disagrees with solidity");
        assert_eq!(gr.logs[0].1, sr.logs[0].1, "log data differs from solidity");
        assert_eq!(U256::from_be_slice(&gr.logs[0].1), U256::from(tag), "the log carries the tag, not a pointer");
    }
}

// The wire form of a uint256[][] body: a count, then one offset per row, then the rows.
fn enc_rows(rows: &[Vec<u64>]) -> Vec<u8> {
    let mut head: Vec<u8> = word_u256(U256::from(rows.len() as u64)).to_vec();
    let mut tail: Vec<u8> = Vec::new();
    let mut cur = 32 * rows.len();
    for r in rows {
        head.extend_from_slice(&word_u256(U256::from(cur as u64)));
        let mut b = word_u256(U256::from(r.len() as u64)).to_vec();
        for v in r {
            b.extend_from_slice(&word_u256(U256::from(*v)));
        }
        cur += b.len();
        tail.extend_from_slice(&b);
    }
    head.extend_from_slice(&tail);
    head
}

// A nested array is the first shape where an element carries an offset instead of its bytes, so the wire grows an indirection the memory form does not have.
#[test]
fn a_nested_array_crosses_the_abi_like_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_NEST_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_NEST_ABI, &solc));

    let rows: Vec<Vec<u64>> = vec![vec![1, 2], vec![10, 20, 30], vec![100]];
    let arr = enc_rows(&rows);

    for f in ["rows", "total", "echo"] {
        let mut d = selector(&format!("{}(uint256[][])", f)).to_vec();
        d.extend_from_slice(&word_u256(U256::from(32u64)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(sr.success, "solidity {} reverted", f);
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
    }

    // By value as well as by diff, so a bug both sides share would still fail.
    let mut d = selector("total(uint256[][])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&arr);
    let gr = call(&mut gdb, g, d);
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(163u64), "1+2+10+20+30+100");

    for (i, want) in [(0u64, 2u64), (1, 3), (2, 1)] {
        let mut d = selector("row_len(uint256[][],uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(64u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "row_len({}) differs from solidity", i);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(want), "row_len({}) by value", i);
    }

    for (i, j, want) in [(0u64, 1u64, 2u64), (1, 2, 30), (2, 0, 100)] {
        let mut d = selector("at(uint256[][],uint256,uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(96u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&word_u256(U256::from(j)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "at({},{}) differs from solidity", i, j);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(want), "at({},{}) by value", i, j);
    }
}

// Three levels deep, a fixed array of dynamic ones, a dynamic array of fixed ones, and both struct-array shapes.
#[test]
fn nested_array_shapes_match_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_NEST_ABI, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_NEST_ABI, &solc));

    macro_rules! both {
        ($d:expr, $what:expr, $want:expr) => {{
            let d: Vec<u8> = $d;
            let gr = call(&mut gdb, g, d.clone());
            let sr = call(&mut sdb, s, d);
            assert!(sr.success, "solidity {} reverted", $what);
            assert!(gr.success, "gum {} reverted", $what);
            assert_eq!(gr.output, sr.output, "{} differs from solidity", $what);
            assert_eq!(U256::from_be_slice(&gr.output), U256::from($want as u64), "{} by value", $what);
        }};
    }

    // uint256[][][]: an offset table whose entries point at offset tables.
    let cubes: Vec<Vec<Vec<u64>>> = vec![vec![vec![1, 2], vec![3]], vec![vec![4, 5, 6]]];
    let mut cube_blob: Vec<u8> = word_u256(U256::from(cubes.len() as u64)).to_vec();
    let mut cube_tail: Vec<u8> = Vec::new();
    let mut cur = 32 * cubes.len();
    for c in &cubes {
        cube_blob.extend_from_slice(&word_u256(U256::from(cur as u64)));
        let b = enc_rows(c);
        cur += b.len();
        cube_tail.extend_from_slice(&b);
    }
    cube_blob.extend_from_slice(&cube_tail);
    for (i, j, k, want) in [(0u64, 0u64, 1u64, 2u64), (0, 1, 0, 3), (1, 0, 2, 6)] {
        let mut d = selector("deep_at(uint256[][][],uint256,uint256,uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(128u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&word_u256(U256::from(j)));
        d.extend_from_slice(&word_u256(U256::from(k)));
        d.extend_from_slice(&cube_blob);
        both!(d, format!("deep_at({},{},{})", i, j, k), want);
    }

    // uint256[][2]: dynamic overall, but the count is in the type, so there is no count word.
    let pair: Vec<Vec<u64>> = vec![vec![7, 8], vec![9]];
    let mut pair_blob: Vec<u8> = Vec::new();
    let mut pair_tail: Vec<u8> = Vec::new();
    let mut pcur = 32 * pair.len();
    for r in &pair {
        pair_blob.extend_from_slice(&word_u256(U256::from(pcur as u64)));
        let mut b = word_u256(U256::from(r.len() as u64)).to_vec();
        for v in r {
            b.extend_from_slice(&word_u256(U256::from(*v)));
        }
        pcur += b.len();
        pair_tail.extend_from_slice(&b);
    }
    pair_blob.extend_from_slice(&pair_tail);
    let mut d = selector("pair_sum(uint256[][2])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&pair_blob);
    both!(d, "pair_sum", 24);

    // uint256[3][]: the element is static, so it is inline with no offset of its own.
    let grid: Vec<[u64; 3]> = vec![[1, 2, 3], [4, 5, 6]];
    let mut grid_blob: Vec<u8> = word_u256(U256::from(grid.len() as u64)).to_vec();
    for r in &grid {
        for v in r {
            grid_blob.extend_from_slice(&word_u256(U256::from(*v)));
        }
    }
    for (i, j, want) in [(0u64, 2u64, 3u64), (1, 0, 4)] {
        let mut d = selector("fixed_rows(uint256[3][],uint256,uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(96u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&word_u256(U256::from(j)));
        d.extend_from_slice(&grid_blob);
        both!(d, format!("fixed_rows({},{})", i, j), want);
    }

    // P[2]: fully static, so it rides inline in the head and the head is four words, not two.
    let mut sp: Vec<u8> = Vec::new();
    for (a, bb) in [(1u64, 111u64), (2, 222)] {
        sp.extend_from_slice(&word_u256(U256::from(a)));
        sp.extend_from_slice(&word_u256(U256::from(bb)));
    }
    for (i, want) in [(0u64, 111u64), (1, 222)] {
        let mut d = selector("struct_pair_b((uint128,uint256)[2],uint256)").to_vec();
        d.extend_from_slice(&sp);
        d.extend_from_slice(&word_u256(U256::from(i)));
        both!(d, format!("struct_pair_b({})", i), want);
    }

    // P[][]: rows of structs, so the offset indirection and the packed-vs-wire field move are both in play at once.
    let sgrid: Vec<Vec<(u64, u64)>> = vec![vec![(1, 10), (2, 20)], vec![(3, 30)]];
    let mut sg: Vec<u8> = word_u256(U256::from(sgrid.len() as u64)).to_vec();
    let mut sg_tail: Vec<u8> = Vec::new();
    let mut scur = 32 * sgrid.len();
    for row in &sgrid {
        sg.extend_from_slice(&word_u256(U256::from(scur as u64)));
        let mut b = word_u256(U256::from(row.len() as u64)).to_vec();
        for (a, bb) in row {
            b.extend_from_slice(&word_u256(U256::from(*a)));
            b.extend_from_slice(&word_u256(U256::from(*bb)));
        }
        scur += b.len();
        sg_tail.extend_from_slice(&b);
    }
    sg.extend_from_slice(&sg_tail);
    for (i, j, want) in [(0u64, 0u64, 10u64), (0, 1, 20), (1, 0, 30)] {
        let mut d = selector("struct_grid_b((uint128,uint256)[][],uint256,uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(96u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&word_u256(U256::from(j)));
        d.extend_from_slice(&sg);
        both!(d, format!("struct_grid_b({},{})", i, j), want);
    }
}

// Event data is an ABI argument list, and the log path used to write one word per field: for an array, a string or a struct that word was the memory pointer.
#[test]
fn logging_a_non_scalar_encodes_it_like_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(GUM_LOG_NONSCALAR, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(SOL_LOG_NONSCALAR, &solc));

    let diff = |gdb: &mut Db, sdb: &mut Db, d: Vec<u8>, what: &str| {
        let gr = call(gdb, g, d.clone());
        let sr = call(sdb, s, d);
        assert!(sr.success, "solidity {} reverted", what);
        assert!(gr.success, "gum {} reverted", what);
        assert_eq!(gr.logs.len(), sr.logs.len(), "{}: log count", what);
        for (i, (gl, sl)) in gr.logs.iter().zip(sr.logs.iter()).enumerate() {
            assert_eq!(gl.0, sl.0, "{}: log {} topics", what, i);
            assert_eq!(gl.1, sl.1, "{}: log {} data", what, i);
        }
        // A pointer-sized word would tie on length with a one-element array, so check the data is really there.
        assert!(!gr.logs.is_empty(), "{}: no log emitted", what);
    };

    // uint256[]
    let mut xs: Vec<u8> = word_u256(U256::from(3u64)).to_vec();
    for v in [11u64, 22, 33] {
        xs.extend_from_slice(&word_u256(U256::from(v)));
    }
    let mut d = selector("arr(uint256[])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&xs);
    diff(&mut gdb, &mut sdb, d, "arr");

    // string
    let text = b"gum";
    let mut sblob: Vec<u8> = word_u256(U256::from(text.len() as u64)).to_vec();
    let mut padded = text.to_vec();
    padded.resize(32, 0);
    sblob.extend_from_slice(&padded);
    let mut d = selector("str(string)").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&sblob);
    diff(&mut gdb, &mut sdb, d, "str");

    // A struct is static, so it rides inline with no offset: getting this one wrong logs the pointer and the field after it.
    let mut d = selector("tup((uint128,uint256))").to_vec();
    d.extend_from_slice(&word_u256(U256::from(7u64)));
    d.extend_from_slice(&word_u256(U256::from(700u64)));
    diff(&mut gdb, &mut sdb, d, "tup");

    // uint256[][], the nested case: the data has an offset table inside an offset.
    let rows: Vec<Vec<u64>> = vec![vec![1, 2], vec![3]];
    let mut d = selector("grid(uint256[][])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&enc_rows(&rows));
    diff(&mut gdb, &mut sdb, d, "grid");

    // Indexed scalar in a topic, two dynamic fields in the data, so the head/tail split has to be right too.
    let who: Address = "0x00000000000000000000000000000000cafebabe".parse().unwrap();
    let mut d = selector("mixed(address,uint256,uint256[],string)").to_vec();
    d.extend_from_slice(&word_addr(who));
    d.extend_from_slice(&word_u256(U256::from(9u64)));
    d.extend_from_slice(&word_u256(U256::from(128u64)));
    d.extend_from_slice(&word_u256(U256::from(128u64 + 32 + 3 * 32)));
    d.extend_from_slice(&xs);
    d.extend_from_slice(&sblob);
    diff(&mut gdb, &mut sdb, d, "mixed");
}

// A custom error's revert data is an ABI argument list too, and its encoder was hand-rolled: it handled scalars and strings and wrote the memory pointer for an array or a struct.
#[test]
fn a_custom_error_with_a_non_scalar_field_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "\
class P:
    u128 a
    u256 b

enum Errors:
    BadArr([u256] xs, u256 n)
    BadTup(P p)
    BadGrid([[u256]] g)

contract C:
    export fn arr([u256] xs) -> u256:
        revert Errors.BadArr(xs, 5)

    export fn tup(P p) -> u256:
        revert Errors.BadTup(p)

    export fn grid([[u256]] g) -> u256:
        revert Errors.BadGrid(g)
";
    let sol_src = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    struct P { uint128 a; uint256 b; }

    error BadArr(uint256[] xs, uint256 n);
    error BadTup(P p);
    error BadGrid(uint256[][] g);

    function arr(uint256[] calldata xs) external pure returns (uint256) {
        revert BadArr(xs, 5);
    }

    function tup(P calldata p) external pure returns (uint256) {
        revert BadTup(p);
    }

    function grid(uint256[][] calldata g) external pure returns (uint256) {
        revert BadGrid(g);
    }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let diff = |gdb: &mut Db, sdb: &mut Db, d: Vec<u8>, what: &str| {
        let gr = call(gdb, g, d.clone());
        let sr = call(sdb, s, d);
        assert!(!gr.success, "gum {} should have reverted", what);
        assert!(!sr.success, "solidity {} should have reverted", what);
        assert_eq!(gr.output, sr.output, "{}: revert data", what);
        // A pointer word is 32 bytes, so a bare selector-plus-one-word would be the bug's shape.
        assert!(gr.output.len() > 4, "{}: revert data is only a selector", what);
    };

    let mut xs: Vec<u8> = word_u256(U256::from(2u64)).to_vec();
    for v in [8u64, 9] {
        xs.extend_from_slice(&word_u256(U256::from(v)));
    }
    let mut d = selector("arr(uint256[])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&xs);
    diff(&mut gdb, &mut sdb, d, "arr");

    let mut d = selector("tup((uint128,uint256))").to_vec();
    d.extend_from_slice(&word_u256(U256::from(4u64)));
    d.extend_from_slice(&word_u256(U256::from(400u64)));
    diff(&mut gdb, &mut sdb, d, "tup");

    let rows: Vec<Vec<u64>> = vec![vec![1], vec![2, 3]];
    let mut d = selector("grid(uint256[][])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&enc_rows(&rows));
    diff(&mut gdb, &mut sdb, d, "grid");
}

// An aggregate local with no initializer used to bind 0, so every read of it went to scratch memory at address 0 rather than to a value of its own.
#[test]
fn an_uninitialized_aggregate_local_is_its_own_zero_value() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "\
use gum.defaults.Account
use gum.defaults.String

class P:
    u128 a
    u256 b

contract C:
    HashMap(Account, u256) m

    export fn seed(Account k, u256 v):
        C.m[k] = v

    export fn arr_zero() -> u256:
        mut [u256; 2] xs
        return xs[0]

    export fn struct_zero() -> u256:
        mut P p
        return p.b

    export fn dyn_len() -> u256:
        mut [u256] xs
        return xs.length

    export fn str_len() -> u256:
        mut String s
        return s.length

    export fn struct_zero_after_map(Account k) -> u256:
        var seen = C.m[k]
        mut P p
        return p.b + seen - seen

    export fn arr_writable() -> u256:
        mut [u256; 2] xs
        xs[1] = 9
        return xs[0] + xs[1]

    export fn delete_struct(Account k) -> u256:
        mut P p
        p.a = 3
        p.b = 77
        delete p
        var dirty = C.m[k]
        return p.b + p.a + dirty - dirty

    export fn delete_dyn([u256] src) -> u256:
        mut [u256] xs = src
        delete xs
        return xs.length

    export fn delete_leaves_neighbour_alone() -> u256:
        mut P p
        mut [u256; 2] after
        after[0] = 8
        p.a = 1
        delete p
        return after[0]
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, true));

    for f in ["arr_zero()", "struct_zero()", "dyn_len()", "str_len()"] {
        let r = call(&mut db, c, selector(f).to_vec());
        assert!(r.success, "{} reverted", f);
        assert_eq!(U256::from_be_slice(&r.output), U256::ZERO, "{} is not zero", f);
    }

    // The one that makes the old bug visible rather than accidentally-zero: seed a mapping so scratch memory holds a live key, then read a fresh struct.
    let key: Address = "0x00000000000000000000000000000000cafebabe".parse().unwrap();
    let r = call(
        &mut db,
        c,
        encode_words("seed(address,uint256)", &[word_addr(key), word_u256(U256::from(4242u64))]),
    );
    assert!(r.success, "seed reverted");
    let r = call(&mut db, c, encode_words("struct_zero_after_map(address)", &[word_addr(key)]));
    assert!(r.success, "struct_zero_after_map reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::ZERO,
        "a fresh struct read scratch memory left over from the mapping lookup"
    );

    // And the block is real memory, not a coincidence: writing one element must not disturb the other.
    let r = call(&mut db, c, selector("arr_writable()").to_vec());
    assert!(r.success, "arr_writable reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(9u64), "0 + 9");

    // delete on a memory-backed local clears its block. Assigning 0 nulled the pointer instead, and a plain read-back would still say zero, since scratch memory is usually zero anyway.
    let r = call(&mut db, c, encode_words("delete_struct(address)", &[word_addr(key)]));
    assert!(r.success, "delete_struct reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::ZERO, "delete left the struct set");

    let mut d = selector("delete_dyn(uint256[])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&word_u256(U256::from(2u64)));
    d.extend_from_slice(&word_u256(U256::from(1u64)));
    d.extend_from_slice(&word_u256(U256::from(2u64)));
    let r = call(&mut db, c, d);
    assert!(r.success, "delete_dyn reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::ZERO, "delete left the array non-empty");

    // A struct is 48 bytes, so its last word is half outside the block: clearing a whole word there would eat the next allocation.
    let r = call(&mut db, c, selector("delete_leaves_neighbour_alone()").to_vec());
    assert!(r.success, "delete_leaves_neighbour_alone reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(8u64), "delete overran into the next allocation");
}

// A struct as a direct contract field: C.p.b = v compiled to a write into a fresh memory copy of the struct, which was then discarded, so the storage write was a silent no-op.
#[test]
fn a_struct_contract_field_persists_like_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "class P:
    u128 a
    u256 b

contract C:
    P p
    u256 tail

    export fn set(u128 a, u256 b):
        C.p.a = a
        C.p.b = b

    export fn get_a() -> u128:
        return C.p.a

    export fn get_b() -> u256:
        return C.p.b

    export fn set_tail(u256 v):
        C.tail = v

    export fn get_tail() -> u256:
        return C.tail
";
    let sol_src = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    struct P { uint128 a; uint256 b; }
    P p;
    uint256 tail;

    function set(uint128 a, uint256 b) external { p.a = a; p.b = b; }
    function get_a() external view returns (uint128) { return p.a; }
    function get_b() external view returns (uint256) { return p.b; }
    function set_tail(uint256 v) external { tail = v; }
    function get_tail() external view returns (uint256) { return tail; }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let d = encode_words("set(uint128,uint256)", &[word_u256(U256::from(7u64)), word_u256(U256::from(12345u64))]);
    assert!(call(&mut gdb, g, d.clone()).success, "gum set reverted");
    assert!(call(&mut sdb, s, d).success, "solidity set reverted");

    // The field written last, and the one packed beside it: a lost write shows up as zero.
    for (f, want) in [("get_b()", 12345u64), ("get_a()", 7)] {
        let gr = call(&mut gdb, g, selector(f).to_vec());
        let sr = call(&mut sdb, s, selector(f).to_vec());
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
        assert_eq!(U256::from_be_slice(&gr.output), U256::from(want), "{} did not persist", f);
    }

    // The field after the struct must not have been displaced by it.
    let d = encode_words("set_tail(uint256)", &[word_u256(U256::from(99u64))]);
    assert!(call(&mut gdb, g, d.clone()).success, "gum set_tail reverted");
    assert!(call(&mut sdb, s, d).success, "solidity set_tail reverted");
    let gr = call(&mut gdb, g, selector("get_tail()").to_vec());
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(99u64), "the field after the struct");
    // And writing it must not have clobbered the struct.
    let gr = call(&mut gdb, g, selector("get_b()").to_vec());
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(12345u64), "tail overlapped the struct");
}

// Vec(P) and [P] are the same storage layout spelled two ways, but only the [P] form reached the struct path: Vec(P) fell through to the packed-scalar reader, so v.get(i).b read a storage word and used it as a memory pointer.
#[test]
fn a_storage_vec_of_structs_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "class P:
    u128 a
    u256 b

contract C:
    Vec(P) v
    [P] xs

    export fn push_v(u128 a, u256 b):
        C.v.push()
        C.v.get(C.v.len() - 1).a = a
        C.v.get(C.v.len() - 1).b = b

    export fn v_len() -> u256:
        return C.v.len()

    export fn v_b(u256 i) -> u256:
        return C.v.get(i).b

    export fn v_a(u256 i) -> u128:
        return C.v.get(i).a
";
    let sol_src = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    struct P { uint128 a; uint256 b; }
    P[] v;
    P[] xs;

    function push_v(uint128 a, uint256 b) external { v.push(P(a, b)); }
    function v_len() external view returns (uint256) { return v.length; }
    function v_b(uint256 i) external view returns (uint256) { return v[i].b; }
    function v_a(uint256 i) external view returns (uint128) { return v[i].a; }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    for (a, b) in [(1u64, 100u64), (2, 200)] {
        let d = encode_words("push_v(uint128,uint256)", &[word_u256(U256::from(a)), word_u256(U256::from(b))]);
        assert!(call(&mut gdb, g, d.clone()).success, "gum push_v reverted");
        assert!(call(&mut sdb, s, d).success, "solidity push_v reverted");
    }

    let gr = call(&mut gdb, g, selector("v_len()").to_vec());
    let sr = call(&mut sdb, s, selector("v_len()").to_vec());
    assert_eq!(gr.output, sr.output, "v_len differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(2u64), "two pushes");

    for (i, wa, wb) in [(0u64, 1u64, 100u64), (1, 2, 200)] {
        for (f, want) in [("v_b(uint256)", wb), ("v_a(uint256)", wa)] {
            let d = encode_words(f, &[word_u256(U256::from(i))]);
            let gr = call(&mut gdb, g, d.clone());
            let sr = call(&mut sdb, s, d);
            assert!(gr.success, "gum {} reverted", f);
            assert_eq!(gr.output, sr.output, "{}[{}] differs from solidity", f, i);
            assert_eq!(U256::from_be_slice(&gr.output), U256::from(want), "{}[{}] by value", f, i);
        }
    }
}

// checked_add and checked_sub guard with unsigned lt, and the "-" arm never branched on signedness at all.
#[test]
fn signed_add_and_sub_match_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "contract C:
    export fn sub(i256 a, i256 b) -> i256:
        return a - b

    export fn add(i256 a, i256 b) -> i256:
        return a + b

    export fn sub8(i8 a, i8 b) -> i8:
        return a - b
";
    let sol_src = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    function sub(int256 a, int256 b) external pure returns (int256) { return a - b; }
    function add(int256 a, int256 b) external pure returns (int256) { return a + b; }
    function sub8(int8 a, int8 b) external pure returns (int8) { return a - b; }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let neg = |v: i64| -> [u8; 32] {
        let u = (v as i128) as u128;
        let mut w = [0xffu8; 32];
        if v >= 0 {
            w = [0u8; 32];
        }
        w[16..].copy_from_slice(&u.to_be_bytes());
        w
    };

    // 1 - 2 = -1, the case that reverted.
    for (f, a, b) in [
        ("sub(int256,int256)", 1i64, 2i64),
        ("sub(int256,int256)", 5, 3),
        ("sub(int256,int256)", -5, 3),
        ("add(int256,int256)", 5, -3),
        ("add(int256,int256)", -5, -3),
        ("add(int256,int256)", 2, 3),
    ] {
        let d = encode_words(f, &[neg(a), neg(b)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.success, sr.success, "{} {} {}: success differs (gum={} sol={})", f, a, b, gr.success, sr.success);
        assert_eq!(gr.output, sr.output, "{} {} {}: differs from solidity", f, a, b);
    }

    let d = encode_words("sub8(int8,int8)", &[neg(1), neg(2)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.success, sr.success, "sub8 1-2: success differs");
    assert_eq!(gr.output, sr.output, "sub8 1-2 differs from solidity");
}

// f32 and f64 are documented and type-checked as WAD fixed point, 1.0 being 10^18, but codegen never scaled them: a  b was a bare mul, so 1.0  1.0 came out as 10^36 rather than 1.0.
#[test]
fn wad_fixed_point_math_matches_hand_scaled_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "contract C:
    export fn mul(f32 a, f32 b) -> f32:
        return a * b

    export fn div(f32 a, f32 b) -> f32:
        return a / b

    export fn add(f32 a, f32 b) -> f32:
        return a + b

    export fn sub(f32 a, f32 b) -> f32:
        return a - b

    export fn scale(f32 a) -> f32:
        return a * 2
";
    let sol_src = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    int256 constant WAD = 1e18;
    function mul(int256 a, int256 b) external pure returns (int256) { return (a * b) / WAD; }
    function div(int256 a, int256 b) external pure returns (int256) { return (a * WAD) / b; }
    function add(int256 a, int256 b) external pure returns (int256) { return a + b; }
    function sub(int256 a, int256 b) external pure returns (int256) { return a - b; }
    function scale(int256 a) external pure returns (int256) { return a * 2; }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let w = |v: i128| -> [u8; 32] {
        let u = v as u128;
        let mut b = if v < 0 { [0xffu8; 32] } else { [0u8; 32] };
        b[16..].copy_from_slice(&u.to_be_bytes());
        b
    };
    const ONE: i128 = 1_000_000_000_000_000_000;

    // 1.0  1.0 must be 1.0. A bare mul gives 10^36.
    let d = encode_words("mul(int256,int256)", &[w(ONE), w(ONE)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum mul reverted");
    assert_eq!(gr.output, sr.output, "1.0 * 1.0 differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(ONE as u128), "1.0 * 1.0 is not 1.0");

    // 2.5  4.0 = 10.0, and a negative operand, which the old unsigned guards would have reverted on.
    for (a, b) in [(ONE * 5 / 2, ONE * 4), (-ONE, ONE * 3), (ONE / 3, ONE * 3)] {
        let d = encode_words("mul(int256,int256)", &[w(a), w(b)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.success, sr.success, "mul {} {}: success differs", a, b);
        assert_eq!(gr.output, sr.output, "mul {} {} differs from solidity", a, b);
    }

    // 1.0 / 4.0 = 0.25, which a bare div would have made 0.
    let d = encode_words("div(int256,int256)", &[w(ONE), w(ONE * 4)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "1.0 / 4.0 differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from((ONE / 4) as u128), "1.0 / 4.0 is not 0.25");

    for (f, a, b) in [
        ("div(int256,int256)", -ONE, ONE * 4),
        ("add(int256,int256)", ONE, -ONE / 2),
        ("sub(int256,int256)", ONE, ONE * 2),
        ("sub(int256,int256)", -ONE, ONE),
    ] {
        let d = encode_words(f, &[w(a), w(b)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.success, sr.success, "{} {} {}: success differs", f, a, b);
        assert_eq!(gr.output, sr.output, "{} {} {} differs from solidity", f, a, b);
    }

    // A bare literal is a plain count, not a fixed-point value, so this doubles a rather than scaling it by 2e-18.
    let d = encode_words("scale(int256)", &[w(ONE * 3)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "scale differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from((ONE * 6) as u128), "3.0 * 2 is not 6.0");

    // Division by zero still panics rather than returning zero the way sdiv would.
    let d = encode_words("div(int256,int256)", &[w(ONE), w(0)]);
    assert!(!call(&mut gdb, g, d).success, "div by zero did not revert");
}

// Returning a dynamic value straight from an external call, return I(t).name(), substituted the call expression twice: once to read the length, once to copy the bytes.
#[test]
fn returning_an_external_dynamic_result_calls_once() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "use gum.defaults.Account
use gum.defaults.String

interface I:
    fn name() -> String

contract C:
    export fn passthrough(Account t) -> String:
        return I(t).name()
";
    // A callee that records how many times it was asked, and returns a string so the caller takes the dynamic path.
    let sol_counter = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract Counter {
    uint256 public calls;
    function name() external returns (string memory) {
        calls += 1;
        return \"gum\";
    }
}
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(gum_src, &solc, false));
    let counter = deploy(&mut db, sol_creation_bytecode(sol_counter, &solc));

    let r = call(&mut db, c, encode_words("passthrough(address)", &[word_addr(counter)]));
    assert!(r.success, "passthrough reverted");

    // The returned string still has to be right, not just the call count.
    let want = {
        let mut w = Vec::new();
        w.extend_from_slice(&word_u256(U256::from(32u64)));
        w.extend_from_slice(&word_u256(U256::from(3u64)));
        let mut s = b"gum".to_vec();
        s.resize(32, 0);
        w.extend_from_slice(&s);
        w
    };
    assert_eq!(r.output, want, "returned string is wrong");

    // calls is at storage slot 0. Exactly one, not two.
    assert_eq!(storage(&mut db, counter, 0), U256::from(1u64), "name() was called more than once");
}

// The type checker knows String.concat and String.slice return a String, but codegen's static_type did not, so it typed the intermediate as unknown and a.concat(b).length fell through to the property-offset catch-all.
#[test]
fn chained_string_method_length_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "use gum.defaults.String

contract C:
    export fn concat_len(String a, String b) -> u256:
        return a.concat(b).length

    export fn slice_len(String a) -> u256:
        return a.slice(1, 4).length
";
    let sol_src = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    function concat_len(string calldata a, string calldata b) external pure returns (uint256) {
        return bytes(string.concat(a, b)).length;
    }
    function slice_len(string calldata a) external pure returns (uint256) {
        return bytes(a[1:4]).length;
    }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, true));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    let enc_str = |bytes: &[u8]| -> Vec<u8> {
        let mut w = word_u256(U256::from(bytes.len() as u64)).to_vec();
        let mut p = bytes.to_vec();
        let pad = (32 - (p.len() % 32)) % 32;
        p.resize(p.len() + pad, 0);
        w.extend_from_slice(&p);
        w
    };

    // concat_len("hello", "world") = 10.
    let a = enc_str(b"hello");
    let b = enc_str(b"world");
    let mut d = selector("concat_len(string,string)").to_vec();
    d.extend_from_slice(&word_u256(U256::from(64u64)));
    d.extend_from_slice(&word_u256(U256::from(64u64 + a.len() as u64)));
    d.extend_from_slice(&a);
    d.extend_from_slice(&b);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum concat_len reverted");
    assert_eq!(gr.output, sr.output, "concat_len differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(10u64), "hello+world length");

    // slice_len of a 6-char string over [1,4) = 3.
    let mut d = selector("slice_len(string)").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&enc_str(b"abcdef"));
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum slice_len reverted");
    assert_eq!(gr.output, sr.output, "slice_len differs from solidity");
    assert_eq!(U256::from_be_slice(&gr.output), U256::from(3u64), "slice [1,4) length");
}

// Child.Ancestor.method() calls that ancestor's version, distinct from Child.method() which is the child's override.
#[test]
fn ancestor_qualified_call_reaches_the_parent_version() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = "class Ledger:
    u256 total

    fn bump() -> u256:
        self.total = self.total + 1
        return self.total

[Ledger]
contract C:
    fn bump() -> u256:
        self.total = self.total + 100
        return self.total

    export fn child_bump() -> u256:
        return C.bump()

    export fn parent_bump() -> u256:
        return C.Ledger.bump()

    export fn get_total() -> u256:
        return C.total
";
    let sol_src = "// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract Ledger {
    uint256 total;
    function bump() public virtual returns (uint256) { total += 1; return total; }
}
contract C is Ledger {
    function bump() public override returns (uint256) { total += 100; return total; }
    function child_bump() external returns (uint256) { return C.bump(); }
    function parent_bump() external returns (uint256) { return Ledger.bump(); }
    function get_total() external view returns (uint256) { return total; }
}
";
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut gdb, gum_creation_bytecode(gum_src, &solc, false));
    let s = deploy(&mut sdb, sol_creation_bytecode(sol_src, &solc));

    macro_rules! both {
        ($sig:expr) => {{
            let d = encode_words($sig, &[]);
            let gr = call(&mut gdb, g, d.clone());
            let sr = call(&mut sdb, s, d);
            assert_eq!(gr.success, sr.success, "success differs for {}", $sig);
            assert_eq!(gr.output, sr.output, "output differs for {}", $sig);
            gr.output
        }};
    }

    // child override adds 100, parent version adds 1, both on the same shared total.
    let o1 = both!("parent_bump()");   // total 0 -> 1
    assert_eq!(U256::from_be_slice(&o1), U256::from(1u64), "parent version should add 1");
    let o2 = both!("child_bump()");    // 1 -> 101
    assert_eq!(U256::from_be_slice(&o2), U256::from(101u64), "child override should add 100");
    let o3 = both!("parent_bump()");   // 101 -> 102
    assert_eq!(U256::from_be_slice(&o3), U256::from(102u64), "parent version again");
    both!("get_total()");
    assert_eq!(storage(&mut gdb, g, 0), U256::from(102u64), "shared total in slot 0");
}

// 512-bit mulDiv: a WAD multiply where the raw product ab overflows int256 but the scaled result (ab)/1e18 fits.
#[test]
fn wad_mul_div_uses_full_precision() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "contract C:
    export fn mul(f32 a, f32 b) -> f32:
        return a * b

    export fn div(f32 a, f32 b) -> f32:
        return a / b
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    let big = |s: &str| U256::from_str_radix(s, 10).unwrap();
    let word = |u: U256| { let mut w = [0u8; 32]; w.copy_from_slice(&u.to_be_bytes::<32>()); w };
    // two's complement of a positive magnitude
    let negw = |u: U256| word((U256::ZERO).wrapping_sub(u));

    // mul: 2e38  3e38 -> 6e58. Raw product 6e76 overflows int256; result fits.
    let a = big("200000000000000000000000000000000000000");
    let b = big("300000000000000000000000000000000000000");
    let want = big("60000000000000000000000000000000000000000000000000000000000");
    let mut d = selector("mul(int256,int256)").to_vec();
    d.extend_from_slice(&word(a)); d.extend_from_slice(&word(b));
    let r = call(&mut db, c, d);
    assert!(r.success, "mul reverted on a product the scaled result can hold");
    assert_eq!(U256::from_be_slice(&r.output), want, "2e38 * 3e38 (WAD) = 6e58");

    // negative: -2e38  3e38 -> -6e58
    let mut d = selector("mul(int256,int256)").to_vec();
    d.extend_from_slice(&negw(a)); d.extend_from_slice(&word(b));
    let r = call(&mut db, c, d);
    assert!(r.success, "signed mul reverted");
    assert_eq!(r.output.as_slice(), negw(want), "-2e38 * 3e38 = -6e58");

    // div: 6e58 / 3e38 -> 2e38. Numerator 6e58  1e18 = 6e76 overflows int256; result fits.
    let n = big("60000000000000000000000000000000000000000000000000000000000");
    let dd = big("300000000000000000000000000000000000000");
    let dwant = big("200000000000000000000000000000000000000");
    let mut d = selector("div(int256,int256)").to_vec();
    d.extend_from_slice(&word(n)); d.extend_from_slice(&word(dd));
    let r = call(&mut db, c, d);
    assert!(r.success, "div reverted on a numerator the result can hold");
    assert_eq!(U256::from_be_slice(&r.output), dwant, "6e58 / 3e38 (WAD) = 2e38");

    // true overflow: 1e58  1e58 / 1e18 = 1e98 > int256 max, must revert.
    let huge = big("10000000000000000000000000000000000000000000000000000000000");
    let mut d = selector("mul(int256,int256)").to_vec();
    d.extend_from_slice(&word(huge)); d.extend_from_slice(&word(huge));
    let r = call(&mut db, c, d);
    assert!(!r.success, "a result exceeding int256 must revert");

    // divide by zero still reverts.
    let mut d = selector("div(int256,int256)").to_vec();
    d.extend_from_slice(&word(big("1000000000000000000"))); d.extend_from_slice(&word(U256::ZERO));
    let r = call(&mut db, c, d);
    assert!(!r.success, "division by zero must revert");
}

#[test]
fn test_try_catch() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let target_src = r#"
enum Errs:
    Failed

contract TheTarget:
    export fn fail():
        revert Errs.Failed
    export fn succeed() -> u256:
        return 42
"#;
    let caller_src = r#"
interface Target:
    fn fail()
    fn succeed() -> u256

contract Caller:
    export fn test_catch(Account t) -> u256:
        try:
            Target(t).fail()
            return 1
        catch:
            return 2

    export fn test_succeed(Account t) -> u256:
        try:
            return Target(t).succeed()
        catch:
            return 0
"#;
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let target = deploy(&mut db, gum_creation_bytecode(target_src, &solc, false));
    let caller = deploy(&mut db, gum_creation_bytecode(caller_src, &solc, false));

    let mut d = selector("test_catch(address)").to_vec();
    let mut addr_bytes = [0u8; 32];
    addr_bytes[12..].copy_from_slice(target.as_slice());
    d.extend_from_slice(&addr_bytes);
    
    let r = call(&mut db, caller, d);
    assert!(r.success);
    assert_eq!(U256::from_be_slice(&r.output), U256::from(2));

    let mut d = selector("test_succeed(address)").to_vec();
    d.extend_from_slice(&addr_bytes);

    let r = call(&mut db, caller, d);
    assert!(r.success);
    assert_eq!(U256::from_be_slice(&r.output), U256::from(42));
}

#[test]
fn test_try_catch_writes_mutation_back() {
    // A try body that mutates a captured variable: on success the new value is
    // written back and visible after the try; on a caught revert the mutation
    // rolls back and catch's assignment stands. bump(n) does n = n + 1 inside
    // the try, guarded by assert(n < 100).
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = r#"
contract C:
    export fn bump(mut u256 n) -> u256:
        try:
            n = n + 1
            assert(n < 100, "cap")
        catch:
            n = 0
        return n
"#;
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    // n = 5: mutation to 6 sticks and is returned.
    let mut d = selector("bump(uint256)").to_vec();
    d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 5; w });
    let r = call(&mut db, c, d);
    assert!(r.success);
    assert_eq!(U256::from_be_slice(&r.output), U256::from(6), "mutation must be written back");

    // n = 200: n+1 trips the assert, the mutation rolls back, catch sets 0.
    let mut d = selector("bump(uint256)").to_vec();
    d.extend_from_slice(&{ let mut w = [0u8; 32]; w[30] = 0; w[31] = 200u8; w });
    let r = call(&mut db, c, d);
    assert!(r.success, "the internal revert must be caught");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(0), "caught path returns catch's value");
}

#[test]
fn test_try_catch_captures_param_and_catches_internal_revert() {
    // The new capability Solidity's try/catch lacks: catch an INTERNAL revert
    // (an assert here, not an external call) while capturing an enclosing
    // parameter and propagating a return. classify(n) tries to require n < 10;
    // for n >= 10 the assert reverts inside the try, is caught, and the function
    // returns 99 from catch. A storage write made before the assert must roll
    // back, which the second call checks.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = r#"
contract C:
    u256 mark

    export fn classify(u256 n) -> u256:
        C.mark = 1
        try:
            C.mark = 2
            assert(n < 10, "too big")
            return n
        catch:
            return 99

    export fn getmark() -> u256:
        return C.mark
"#;
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    // n = 5: the try succeeds, returns n, and the write to 2 sticks.
    let mut d = selector("classify(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 5;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success);
    assert_eq!(U256::from_be_slice(&r.output), U256::from(5));
    let r = call(&mut db, c, selector("getmark()").to_vec());
    assert_eq!(U256::from_be_slice(&r.output), U256::from(2), "success keeps the write");

    // n = 20: the assert reverts inside the try, is caught (returns 99), and the
    // write to 2 rolls back to the pre-try 1.
    let mut d = selector("classify(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 20;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success, "internal revert must be caught, not bubble out");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(99));
    let r = call(&mut db, c, selector("getmark()").to_vec());
    assert_eq!(U256::from_be_slice(&r.output), U256::from(1), "caught revert must roll back the write");
}

#[test]
fn test_try_catch_captures_a_local_and_catches_internal_revert() {
    // The try body captures an enclosing *local* (n) rather than a parameter.
    // This used to fall back to the external-call-only path and NOT catch the
    // internal assert; now the local is marshalled into the frame like a
    // parameter, so the assert is caught and the pre-assert storage write rolls
    // back. Runs the same checks for both spellings of the local — an explicit
    // type and an inferred `var` — to lock in that they behave *identically*
    // (the inferred type is resolved into the AST before the try is hoisted, so
    // neither silently downgrades to the weaker path).
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    for decl in ["u256 n = arg", "var n = arg"] {
        let src = include_str!("fixtures/gum_try_local.gum").replace("__DECL__", decl);
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let c = deploy(&mut db, gum_creation_bytecode(&src, &solc, false));

        // arg = 5: the try succeeds, returns n, the write to 2 sticks.
        let mut d = selector("classify(uint256)").to_vec();
        d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 5; w });
        let r = call(&mut db, c, d);
        assert!(r.success, "[{}] classify(5) reverted", decl);
        assert_eq!(U256::from_be_slice(&r.output), U256::from(5), "[{}] captured local should reach the body", decl);
        let r = call(&mut db, c, selector("getmark()").to_vec());
        assert_eq!(U256::from_be_slice(&r.output), U256::from(2), "[{}] success keeps the write", decl);

        // arg = 20: the assert reverts inside the try, is caught (returns 99), and
        // the write to 2 rolls back to the pre-try 1 — proof the local took the
        // internal-revert-catching path, not the old external-only one.
        let mut d = selector("classify(uint256)").to_vec();
        d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 20; w });
        let r = call(&mut db, c, d);
        assert!(r.success, "[{}] internal revert must be caught with a captured local, not bubble out", decl);
        assert_eq!(U256::from_be_slice(&r.output), U256::from(99), "[{}] catch should return 99", decl);
        let r = call(&mut db, c, selector("getmark()").to_vec());
        assert_eq!(U256::from_be_slice(&r.output), U256::from(1), "[{}] caught revert must roll back the write", decl);
    }
}

#[test]
fn test_try_catch_writes_back_a_string_of_any_type() {
    // Write-back is no longer limited to one-word value types: a captured String
    // the body reassigns travels back out of the frame on success, decoded through
    // the String codec. On a caught revert the frame rolls back, so the reassigned
    // value is gone and catch's assignment stands. Solidity's try can't express
    // any of this, so this is checked by gum's own behaviour.
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/gum_try_string_wb.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    // n = 3: the try succeeds, the reassigned String is written back and returned.
    let mut d = selector("label(uint256)").to_vec();
    d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 3; w });
    let r = call(&mut db, c, d);
    assert!(r.success, "label(3) reverted");
    assert_eq!(
        r.output,
        abi_encode_string(b"updated-to-a-long-value-past-31-bytes-so-it-goes-long-form"),
        "success must write back the mutated String"
    );

    // n = 20: the assert reverts inside the try, rolls the reassignment back, and
    // catch sets s = "caught".
    let mut d = selector("label(uint256)").to_vec();
    d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 20; w });
    let r = call(&mut db, c, d);
    assert!(r.success, "internal revert must be caught");
    assert_eq!(r.output, abi_encode_string(b"caught"), "caught path returns the catch value");
}

#[test]
fn test_nested_try_catches_at_each_level() {
    // A try nested inside another try: the inner one catches an internal revert
    // and its result propagates out, so the outer catch is never reached. Proves
    // nested trys hoist and compose (each becomes its own guarded frame).
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/gum_try_nested.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    // a = 2: inner assert holds, inner try returns 1.
    let mut d = selector("f(uint256)").to_vec();
    d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 2; w });
    let r = call(&mut db, c, d);
    assert!(r.success);
    assert_eq!(U256::from_be_slice(&r.output), U256::from(1), "inner success path");

    // a = 9: inner assert reverts, inner catch returns 2 (outer catch untouched).
    let mut d = selector("f(uint256)").to_vec();
    d.extend_from_slice(&{ let mut w = [0u8; 32]; w[31] = 9; w });
    let r = call(&mut db, c, d);
    assert!(r.success, "inner internal revert must be caught by the inner catch");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(2), "inner catch path");
}

// A number literal is u256 by default, so it must not set the arithmetic width
// when the other operand is narrower: 2 * v on a u8 has to use a u8 overflow
// bound, not a 256-bit one, or it wraps instead of reverting. Regression for a
// bug where the literal's position (left vs right) changed the overflow check.
#[test]
fn narrow_arithmetic_overflow_is_independent_of_literal_position() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum = gum_creation_bytecode(include_str!("fixtures/gum_arith_literal_pos.gum"), &solc, false);
    let sol = sol_creation_bytecode(include_str!("fixtures/sol_arith_literal_pos.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    let w = |v: u64| {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&v.to_be_bytes());
        b
    };

    // The u256 case (the plain one): a * 2 and a * b both give 20, no revert.
    assert!(call(&mut gdb, ga, encode_words("seta(uint256)", &[w(10)])).success);
    assert!(call(&mut sdb, sa, encode_words("seta(uint256)", &[w(10)])).success);
    for sig in ["a_lit()", "a_var(uint256)"] {
        let args = if sig == "a_lit()" { vec![] } else { vec![w(2)] };
        let g = call(&mut gdb, ga, encode_words(sig, &args));
        let s = call(&mut sdb, sa, encode_words(sig, &args));
        assert!(g.success && s.success, "{} reverted", sig);
        assert_eq!(g.output, s.output, "{} differs", sig);
        assert_eq!(U256::from_be_slice(&g.output), U256::from(20u64), "{} wrong", sig);
    }

    // Narrow overflow (u8: 200*2=400, i8: 100*2=200) must revert in every operand
    // position, exactly as Solidity does, regardless of where the literal sits or
    // whether the type is signed.
    for (sig, args) in [
        ("u8_lit_r(uint8)", vec![w(200)]),
        ("u8_lit_l(uint8)", vec![w(200)]),
        ("u8_var(uint8,uint8)", vec![w(200), w(2)]),
        ("i8_lit_r(int8)", vec![w(100)]),
        ("i8_lit_l(int8)", vec![w(100)]),
        ("i8_var(int8,int8)", vec![w(100), w(2)]),
    ] {
        let g = call(&mut gdb, ga, encode_words(sig, &args));
        let s = call(&mut sdb, sa, encode_words(sig, &args));
        assert_eq!(g.success, s.success, "{}: revert must agree with Solidity", sig);
        assert!(!g.success, "{}: narrow overflow must revert", sig);
    }
}
