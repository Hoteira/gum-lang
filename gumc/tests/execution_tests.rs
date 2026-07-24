use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use revm::context::TxEnv;
use revm::context::result::{ExecutionResult, Output};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::{Address, TxKind, U256, hardfork::SpecId};
use revm::{Context, Database, ExecuteCommitEvm, MainBuilder, MainContext};

type Db = CacheDB<EmptyDB>;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

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
    if Command::new("solc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
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

fn gum_creation_bytecode(src: &str, solc: &Path, rich: bool) -> Vec<u8> {
    gum_creation_bytecode_for(src, solc, rich, "")
}

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

fn gum_creation_bytecode_for(src: &str, solc: &Path, rich: bool, name: &str) -> Vec<u8> {
    let path = tmp_path("gum");
    std::fs::write(&path, src).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gumc"));
    cmd.arg(&path).arg("--bytecode").arg("--solc").arg(solc);
    if rich {
        cmd.arg("--rich-reverts");
    }
    let out = cmd.output().expect("failed to run gumc");
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
    let is_hex = |l: &str| {
        l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit())
    };

    let mut lines = text.lines().map(str::trim);
    if !name.is_empty() {
        let banner = format!("[Assembler] {} EVM bytecode", name);
        lines.find(|l| l.contains(&banner)).unwrap_or_else(|| {
            panic!(
                "gumc emitted no bytecode for '{}':\n{}{}",
                name,
                text,
                String::from_utf8_lossy(&out.stderr)
            )
        });
    }
    let hex = lines.find(|l| is_hex(l)).unwrap_or_else(|| {
        panic!(
            "no bytecode from gumc:\n{}{}",
            text,
            String::from_utf8_lossy(&out.stderr)
        )
    });
    hex::decode(&hex[2..]).expect("bad gum hex")
}

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
        .find(|l| {
            l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit())
        })
        .unwrap_or_else(|| {
            panic!(
                "no bytecode from locked gumc:\n{}{}",
                text,
                String::from_utf8_lossy(&out.stderr)
            )
        });
    hex::decode(&hex[2..]).expect("bad gum hex")
}

fn sol_creation_bytecode(src: &str, solc: &Path) -> Vec<u8> {
    sol_creation_bytecode_for(src, solc, "")
}

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
        lines.find(|l| l.contains(&banner)).unwrap_or_else(|| {
            panic!(
                "solc emitted no bytecode for '{}':\n{}{}",
                name,
                text,
                String::from_utf8_lossy(&out.stderr)
            )
        });
    }
    let hex = lines
        .find(|l| l.len() > 40 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            panic!(
                "no bytecode from solc:\n{}{}",
                text,
                String::from_utf8_lossy(&out.stderr)
            )
        });
    hex::decode(hex).expect("bad sol hex")
}

fn deployer() -> Address {
    Address::from([0x11u8; 20])
}

const TX_GAS_LIMIT: u64 = 16_777_216;

macro_rules! evm_for {
    ($db:expr) => {
        Context::mainnet()
            .with_db(&mut *$db)
            .modify_cfg_chained(|c| {
                c.spec = SpecId::OSAKA;

                c.disable_nonce_check = true;
            })
            .build_mainnet()
    };
}

fn deploy(db: &mut Db, creation: Vec<u8>) -> Address {
    deploy_with_gas(db, creation).0
}

fn try_deploy(db: &mut Db, creation: Vec<u8>) -> Option<Address> {
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
        ExecutionResult::Success {
            output: Output::Create(_, Some(addr)),
            ..
        } => Some(addr),
        _ => None,
    }
}

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
        ExecutionResult::Success {
            output: Output::Create(_, Some(addr)),
            gas,
            ..
        } => (addr, gas.tx_gas_used()),
        other => panic!("deployment did not create a contract: {:?}", other),
    }
}

struct CallResult {
    success: bool,
    output: Vec<u8>,

    logs: Vec<(Vec<[u8; 32]>, Vec<u8>)>,

    gas: u64,
}

fn call(db: &mut Db, to: Address, data: Vec<u8>) -> CallResult {
    call_from(db, deployer(), to, data)
}

fn call_from(db: &mut Db, caller: Address, to: Address, data: Vec<u8>) -> CallResult {
    call_with_value(db, caller, to, data, U256::ZERO)
}

fn call_with_value(
    db: &mut Db,
    caller: Address,
    to: Address,
    data: Vec<u8>,
    value: U256,
) -> CallResult {
    if value > U256::ZERO {
        let mut info = db.basic(caller).unwrap().unwrap_or_default();
        info.balance = info
            .balance
            .saturating_add(value)
            .saturating_add(U256::from(10u64).pow(U256::from(18)));
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
        ExecutionResult::Success {
            output, logs, gas, ..
        } => {
            let logs = logs
                .into_iter()
                .map(|l| {
                    let topics = l.data.topics().iter().map(|t| t.0).collect();
                    (topics, l.data.data.to_vec())
                })
                .collect();
            CallResult {
                success: true,
                output: output.into_data().to_vec(),
                logs,
                gas: gas.tx_gas_used(),
            }
        }
        ExecutionResult::Revert { output, gas, .. } => CallResult {
            success: false,
            output: output.to_vec(),
            logs: vec![],
            gas: gas.tx_gas_used(),
        },
        ExecutionResult::Halt { gas, .. } => CallResult {
            success: false,
            output: vec![],
            logs: vec![],
            gas: gas.tx_gas_used(),
        },
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
    db.storage(addr, U256::from(slot))
        .expect("storage read failed")
}

fn storage_at(db: &mut Db, addr: Address, slot: U256) -> U256 {
    db.storage(addr, slot).expect("storage read failed")
}

fn mapping_slot(key: Address, base: u8) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_slice());
    buf[63] = base;
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(&buf);
    k.finalize(&mut out);
    U256::from_be_bytes(out)
}

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn next_u256(&mut self, wide: bool) -> U256 {
        if wide && self.next_u64() % 4 == 0 {
            U256::MAX - U256::from(self.next_u64())
        } else {
            U256::from(self.next_u64())
        }
    }
}

const GUM_ARR_COPY: &str = include_str!("fixtures/storage/array_copy.gum");
const GUM_STRUCT_ABI: &str = include_str!("fixtures/abi/struct.gum");
const GUM_STRUCT_CTOR: &str = include_str!("fixtures/struct/ctor.gum");
const GUM_STRUCT_DEPLOY: &str = include_str!("fixtures/struct/deploy.gum");
const GUM_IFACE_CALL: &str = include_str!("fixtures/abi/iface_call.gum");
const GUM_MSG_BLOCK: &str = include_str!("fixtures/context/msg_block.gum");
const GUM_ENUM_ABI: &str = include_str!("fixtures/enum/abi.gum");
const GUM_ENUM_STATE: &str = include_str!("fixtures/enum/state.gum");
const SOL_ENUM_STATE: &str = include_str!("fixtures/enum/state.sol");
const GUM_VIEW: &str = include_str!("fixtures/context/view.gum");
const SOL_PROBER: &str = include_str!("fixtures/abi/prober.sol");
const SOL_ENUM_ABI: &str = include_str!("fixtures/enum/abi.sol");
const SOL_MSG_BLOCK: &str = include_str!("fixtures/context/msg_block.sol");
const GUM_STARR_ABI: &str = include_str!("fixtures/abi/struct_array.gum");
const GUM_NEST_ABI: &str = include_str!("fixtures/abi/nested_array.gum");
const GUM_LOG_NONSCALAR: &str = include_str!("fixtures/abi/log_nonscalar.gum");
const SOL_LOG_NONSCALAR: &str = include_str!("fixtures/abi/log_nonscalar.sol");
const SOL_NEST_ABI: &str = include_str!("fixtures/abi/nested_array.sol");
const SOL_STARR_ABI: &str = include_str!("fixtures/abi/struct_array.sol");
const SOL_IFACE_SINK: &str = include_str!("fixtures/abi/iface_sink.sol");
const SOL_STRUCT_DEPLOY: &str = include_str!("fixtures/struct/deploy.sol");
const SOL_STRUCT_CTOR: &str = include_str!("fixtures/struct/ctor.sol");
const SOL_STRUCT_ABI: &str = include_str!("fixtures/abi/struct.sol");
const GUM_STORE: &str = include_str!("fixtures/storage/store.gum");

const SOL_STORE: &str = include_str!("fixtures/storage/store.sol");

const GUM_STRING_ECHO: &str = include_str!("fixtures/string/echo.gum");

const SOL_STRING_ECHO: &str = include_str!("fixtures/string/echo.sol");

const GUM_STORE_CONSTRUCTOR: &str = include_str!("fixtures/storage/store_constructor.gum");

const GUM_ABI_MIX: &str = include_str!("fixtures/abi/mix.gum");

const SOL_ABI_MIX: &str = include_str!("fixtures/abi/mix.sol");

const SOL_STORE_CONSTRUCTOR: &str = include_str!("fixtures/storage/store_constructor.sol");

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

        assert_eq!(
            gr.success, sr.success,
            "success mismatch on {}: gum={} sol={}",
            sig, gr.success, sr.success
        );
        assert_eq!(
            gr.output, sr.output,
            "return-data mismatch on {}:\n gum={:02x?}\n sol={:02x?}",
            sig, gr.output, sr.output
        );

        let gs = storage(&mut gdb, gaddr, 0);
        let ss = storage(&mut sdb, saddr, 0);
        assert_eq!(
            gs, ss,
            "slot-0 storage mismatch after {}: gum={} sol={}",
            sig, gs, ss
        );
    }
}

#[test]
fn store_set_add_get_matches_solidity() {
    diff_run(
        &[
            ("set(uint256)", vec![U256::from(5)]),
            ("add(uint256)", vec![U256::from(37)]),
            ("get()", vec![]),
        ],
        false,
    );
}

#[test]
fn store_get_returns_correct_value() {
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
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(99),
        "gum get() returned wrong value"
    );
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
    assert_eq!(
        gr.output, sr.output,
        "gum and sol get() return data mismatch"
    );
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(999),
        "constructor did not initialize storage correctly"
    );
}

#[test]
fn string_abi_decoding_and_encoding_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());

    let gaddr = deploy(
        &mut gdb,
        gum_creation_bytecode(GUM_STRING_ECHO, &solc, false),
    );
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STRING_ECHO, &solc));

    let message = b"Hello, Gum World! This is a dynamic string!";
    let mut data = selector("echo(string)").to_vec();
    data.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(message.len()).to_be_bytes::<32>());
    data.extend_from_slice(message);

    let pad_len = (32 - (message.len() % 32)) % 32;
    data.extend(vec![0u8; pad_len]);

    let gr = call(&mut gdb, gaddr, data.clone());
    let sr = call(&mut sdb, saddr, data.clone());

    assert!(gr.success, "gum echo() reverted");
    assert!(sr.success, "sol echo() reverted");
    assert_eq!(
        gr.output, sr.output,
        "gum and sol echo() return data mismatch"
    );

    let mut data2 = selector("get_len(string)").to_vec();
    data2.extend_from_slice(&data[4..]);

    let gr2 = call(&mut gdb, gaddr, data2.clone());
    let sr2 = call(&mut sdb, saddr, data2);

    assert!(gr2.success, "gum get_len() reverted");
    assert!(sr2.success, "sol get_len() reverted");
    assert_eq!(
        gr2.output, sr2.output,
        "gum and sol get_len() return data mismatch"
    );
}

#[test]
fn short_calldata_reverts_like_solidity() {
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

    let bare_selector = selector("set(uint256)").to_vec();
    let gr = call(&mut gdb, gaddr, bare_selector.clone());
    let sr = call(&mut sdb, saddr, bare_selector);

    assert!(
        !sr.success,
        "sanity: Solidity should revert on short calldata"
    );
    assert_eq!(
        gr.success, sr.success,
        "gum must also revert on short calldata (got success={})",
        gr.success
    );
}

const SOL_MOCK: &str = include_str!("fixtures/abi/mock.sol");

fn assert_logs_match(sig: &str, g: &CallResult, s: &CallResult) {
    assert_eq!(
        g.logs.len(),
        s.logs.len(),
        "log count mismatch after {}: gum={} sol={}",
        sig,
        g.logs.len(),
        s.logs.len()
    );
    for (i, (gl, sl)) in g.logs.iter().zip(s.logs.iter()).enumerate() {
        assert_eq!(gl.0, sl.0, "log {} topics differ after {}", i, sig);
        assert_eq!(gl.1, sl.1, "log {} data differ after {}", i, sig);
    }
}

#[test]
fn amm_external_calls_storage_and_events_match_solidity() {
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

    assert_eq!((ga, gb, gamm), (sa, sb, samm), "deploy addresses diverged");

    let steps: Vec<(&str, Vec<[u8; 32]>)> = vec![
        (
            "initialize(address,address)",
            vec![word_addr(ga), word_addr(gb)],
        ),
        (
            "add_liquidity(uint256,uint256)",
            vec![word_u256(U256::from(1000)), word_u256(U256::from(2000))],
        ),
        (
            "swap(address,uint256)",
            vec![word_addr(ga), word_u256(U256::from(500))],
        ),
    ];

    for (sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call(&mut gdb, gamm, data.clone());
        let sr = call(&mut sdb, samm, data);

        assert_eq!(
            gr.success, sr.success,
            "success mismatch on {}: gum={} sol={}",
            sig, gr.success, sr.success
        );
        assert!(gr.success, "{} reverted unexpectedly on gum", sig);
        assert_eq!(gr.output, sr.output, "return data mismatch on {}", sig);
        assert_logs_match(sig, &gr, &sr);

        for slot in [2u64, 3, 4] {
            let gs = storage(&mut gdb, gamm, slot);
            let ss = storage(&mut sdb, samm, slot);
            assert_eq!(
                gs, ss,
                "storage slot {} mismatch after {}: gum={} sol={}",
                slot, sig, gs, ss
            );
        }

        let mslot = mapping_slot(deployer(), 5);
        let gm = storage_at(&mut gdb, gamm, mslot);
        let sm = storage_at(&mut sdb, samm, mslot);
        assert_eq!(
            gm, sm,
            "shares[sender] mapping slot mismatch after {}: gum={} sol={}",
            sig, gm, sm
        );
    }

    assert_eq!(
        storage_at(&mut gdb, gamm, mapping_slot(deployer(), 5)),
        U256::from(1000),
        "shares[sender]"
    );

    assert_eq!(storage(&mut gdb, gamm, 2), U256::from(1500), "reserve_a");
    assert_eq!(
        storage(&mut gdb, gamm, 3),
        U256::from(2000u64 - (2000u64 * 500 / 1500)),
        "reserve_b"
    );
}

#[test]
fn overflow_reverts_with_matching_panic_data() {
    diff_run(
        &[
            ("set(uint256)", vec![U256::MAX]),
            ("add(uint256)", vec![U256::from(1)]),
        ],
        true,
    );
}

const GUM_MAP_CSE: &str = include_str!("fixtures/map/cse.gum");

const SOL_MAP_CSE: &str = include_str!("fixtures/map/cse.sol");

const GUM_PACK: &str = include_str!("fixtures/storage/pack.gum");

const SOL_PACK: &str = include_str!("fixtures/storage/pack.sol");

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
    let args = encode_words(
        "setall(uint128,uint256,uint128)",
        &[
            word_u256(U256::from(7u64)),
            word_u256(U256::from(9u64)),
            word_u256(U256::from(11u64)),
        ],
    );
    let g = call(&mut gdb, gaddr, args.clone());
    let s = call(&mut sdb, saddr, args);
    println!(
        "\n[probe] setall (3 fields):  gum {}  sol {}  (delta {:+})",
        g.gas,
        s.gas,
        g.gas as i64 - s.gas as i64
    );
    assert!(
        g.success && s.success,
        "gum={} sol={}",
        g.success,
        s.success
    );
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
    println!(
        "\n[probe] mapping read x5:  gum {}  sol {}  (delta {:+})",
        g.gas,
        s.gas,
        g.gas as i64 - s.gas as i64
    );
    assert!(g.success && s.success);
}

const LOCK_V1: &str = include_str!("fixtures/misc/lock_v1.gum");

const LOCK_V2: &str = include_str!("fixtures/misc/lock_v2.gum");

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

    let v1 = gum_creation_bytecode_locked(LOCK_V1, &solc, &lock);
    let v2 = gum_creation_bytecode_locked(LOCK_V2, &solc, &lock);
    let _ = std::fs::remove_file(&lock);

    let mut db1: Db = CacheDB::new(EmptyDB::default());
    let a1 = deploy(&mut db1, v1);
    call(
        &mut db1,
        a1,
        encode(
            "set_ab(uint128,uint128)",
            &[U256::from(7u64), U256::from(11u64)],
        ),
    );
    let v1_slot0 = storage(&mut db1, a1, 0);
    assert_eq!(
        U256::from_be_slice(&call(&mut db1, a1, encode("get_a()", &[])).output),
        U256::from(7u64)
    );

    let mut db2: Db = CacheDB::new(EmptyDB::default());
    let a2 = deploy(&mut db2, v2);
    db2.insert_account_storage(a2, U256::from(0u64), v1_slot0)
        .unwrap();

    let ga = U256::from_be_slice(&call(&mut db2, a2, encode("get_a()", &[])).output);
    let gb = U256::from_be_slice(&call(&mut db2, a2, encode("get_b()", &[])).output);
    let gbig = U256::from_be_slice(&call(&mut db2, a2, encode("get_big()", &[])).output);
    assert_eq!(
        ga,
        U256::from(7u64),
        "v2 misread v1's a, layout drifted despite the lock"
    );
    assert_eq!(
        gb,
        U256::from(11u64),
        "v2 misread v1's b, layout drifted despite the lock"
    );
    assert_eq!(
        gbig,
        U256::ZERO,
        "appended field big should occupy a fresh, zero slot"
    );
}

fn print_gas_row(label: &str, gum: u64, sol: u64) {
    let delta = gum as i64 - sol as i64;
    let pct = if sol > 0 {
        gum as f64 / sol as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "  {:<30} gum {:>7}   sol {:>7}   delta {:>+6}   ({:.0}% of sol)",
        label, gum, sol, delta, pct
    );
}

fn gas_contract(
    solc: &Path,
    name: &str,
    gum_src: &str,
    sol_src: &str,
    steps: &[(&str, Vec<[u8; 32]>)],
) {
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

#[test]
fn size_report() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping size report: no solc");
            return;
        }
    };

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

        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let gaddr = deploy(&mut gdb, gum_creation_bytecode(&gum_src, &solc, false));
        let saddr = deploy(&mut sdb, sol_creation_bytecode(&sol_src, &solc));
        let g = gdb.basic(gaddr).unwrap().unwrap().code.unwrap().len() as u64;
        let s = sdb.basic(saddr).unwrap().unwrap().code.unwrap().len() as u64;
        let pct = g * 100 / s;
        lo = lo.min(pct);
        hi = hi.max(pct);
        println!(
            "  {:<8} gum {:>5}   sol {:>5}   {:>3}% of sol",
            name, g, s, pct
        );
    }
    println!("  range: {}-{}% of Solidity", lo, hi);

    assert!(
        lo >= 50 && hi <= 130,
        "size range {}-{}% is outside the documented 50-130% band",
        lo,
        hi
    );
}

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
            (
                "initialize(uint256)",
                vec![word_u256(U256::from(1_000_000u64))],
            ),
            (
                "transfer(address,uint256)",
                vec![word_addr(to), word_u256(U256::from(100u64))],
            ),
            (
                "mint(address,uint256)",
                vec![word_addr(to), word_u256(U256::from(50u64))],
            ),
        ];
        for (sig, words) in &steps {
            let g = call(&mut gdb, gaddr, encode_words(sig, words));
            let s = call(&mut sdb, saddr, encode_words(sig, words));
            assert!(
                g.success && s.success,
                "{} failed (gum={}, sol={})",
                sig,
                g.success,
                s.success
            );
            print_gas_row(sig, g.gas, s.gas);
        }
    }

    let sp = word_addr(Address::from([0x31u8; 20]));
    gas_contract(
        &solc,
        "erc20",
        &read_repo_file("examples/erc20.gum"),
        &read_repo_file("examples/solidity/erc20.sol"),
        &[
            ("init(uint256)", vec![word_u256(U256::from(1_000_000u64))]),
            (
                "approve(address,uint256)",
                vec![sp, word_u256(U256::from(500u64))],
            ),
            (
                "transfer(address,uint256)",
                vec![sp, word_u256(U256::from(100u64))],
            ),
        ],
    );
    gas_contract(
        &solc,
        "erc721",
        &read_repo_file("examples/erc721.gum"),
        &read_repo_file("examples/solidity/erc721.sol"),
        &[
            (
                "mint(address,uint256)",
                vec![word_addr(deployer()), word_u256(U256::from(1u64))],
            ),
            (
                "approve(address,uint256)",
                vec![sp, word_u256(U256::from(1u64))],
            ),
            (
                "setApprovalForAll(address,bool)",
                vec![sp, word_u256(U256::from(1u64))],
            ),
        ],
    );
    gas_contract(
        &solc,
        "vault",
        &read_repo_file("examples/vault.gum"),
        &read_repo_file("examples/solidity/vault.sol"),
        &[
            (
                "deposit(uint256,uint256)",
                vec![
                    word_u256(U256::from(100u64)),
                    word_u256(U256::from(5000u64)),
                ],
            ),
            ("withdraw(uint256)", vec![word_u256(U256::from(30u64))]),
        ],
    );
    gas_contract(
        &solc,
        "dyn_array",
        GUM_DYN_ARRAY,
        SOL_DYN_ARRAY,
        &[
            ("push_val(uint256)", vec![word_u256(U256::from(7u64))]),
            ("push_val(uint256)", vec![word_u256(U256::from(8u64))]),
            (
                "set_at(uint256,uint256)",
                vec![word_u256(U256::from(0u64)), word_u256(U256::from(9u64))],
            ),
        ],
    );

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
            (
                "initialize(address,address)",
                vec![word_addr(ga), word_addr(gb)],
            ),
            (
                "add_liquidity(uint256,uint256)",
                vec![
                    word_u256(U256::from(1000u64)),
                    word_u256(U256::from(2000u64)),
                ],
            ),
            (
                "swap(address,uint256)",
                vec![word_addr(ga), word_u256(U256::from(500u64))],
            ),
        ];
        for (sig, words) in &steps {
            let g = call(&mut gdb, gamm, encode_words(sig, words));
            let s = call(&mut sdb, samm, encode_words(sig, words));
            assert!(
                g.success && s.success,
                "{} failed (gum={}, sol={})",
                sig,
                g.success,
                s.success
            );
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
    let g = gum.as_nanos() as f64 / reps as f64 / 1000.0;
    let s = sol.as_nanos() as f64 / reps as f64 / 1000.0;
    println!(
        "  {:<26} gum {:>8.2}us   sol {:>8.2}us   ({:.0}% of sol)",
        label,
        g,
        s,
        g / s * 100.0
    );
}

fn time_contract(
    solc: &Path,
    name: &str,
    gum_src: &str,
    sol_src: &str,
    steps: &[(&str, Vec<[u8; 32]>)],
    reps: u32,
) {
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
    println!(
        "\n===== EXECUTION TIME (revm wall-clock, avg of {} reps) =====",
        REPS
    );

    {
        let gum = gum_creation_bytecode(&read_repo_file("examples/token.gum"), &solc, false);
        let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/token.sol"), &solc);
        let to = Address::from([0x55u8; 20]);
        let init = encode_words(
            "initialize(uint256)",
            &[word_u256(U256::from(1_000_000u64))],
        );
        let xfer = encode_words(
            "transfer(address,uint256)",
            &[word_addr(to), word_u256(U256::from(100u64))],
        );
        let mint = encode_words(
            "mint(address,uint256)",
            &[word_addr(to), word_u256(U256::from(50u64))],
        );

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
    time_contract(
        &solc,
        "erc20 (deploy+3)",
        &read_repo_file("examples/erc20.gum"),
        &read_repo_file("examples/solidity/erc20.sol"),
        &[
            ("init(uint256)", vec![word_u256(U256::from(1_000_000u64))]),
            (
                "approve(address,uint256)",
                vec![sp, word_u256(U256::from(500u64))],
            ),
            (
                "transfer(address,uint256)",
                vec![sp, word_u256(U256::from(100u64))],
            ),
        ],
        REPS,
    );
    time_contract(
        &solc,
        "erc721 (deploy+2)",
        &read_repo_file("examples/erc721.gum"),
        &read_repo_file("examples/solidity/erc721.sol"),
        &[
            (
                "mint(address,uint256)",
                vec![word_addr(deployer()), word_u256(U256::from(1u64))],
            ),
            (
                "approve(address,uint256)",
                vec![sp, word_u256(U256::from(1u64))],
            ),
        ],
        REPS,
    );
    time_contract(
        &solc,
        "vault (deploy+2)",
        &read_repo_file("examples/vault.gum"),
        &read_repo_file("examples/solidity/vault.sol"),
        &[
            (
                "deposit(uint256,uint256)",
                vec![
                    word_u256(U256::from(100u64)),
                    word_u256(U256::from(5000u64)),
                ],
            ),
            ("withdraw(uint256)", vec![word_u256(U256::from(30u64))]),
        ],
        REPS,
    );

    {
        let mock = sol_creation_bytecode(SOL_MOCK, &solc);
        let gum = gum_creation_bytecode(&read_repo_file("examples/amm.gum"), &solc, false);
        let sol = sol_creation_bytecode(&read_repo_file("examples/solidity/amm.sol"), &solc);

        let run = |amm: &Vec<u8>| {
            let mut db: Db = CacheDB::new(EmptyDB::default());
            let a = deploy(&mut db, mock.clone());
            let b = deploy(&mut db, mock.clone());
            let amm_addr = deploy(&mut db, amm.clone());
            call(
                &mut db,
                amm_addr,
                encode_words("initialize(address,address)", &[word_addr(a), word_addr(b)]),
            );
            call(
                &mut db,
                amm_addr,
                encode_words(
                    "add_liquidity(uint256,uint256)",
                    &[
                        word_u256(U256::from(1000u64)),
                        word_u256(U256::from(2000u64)),
                    ],
                ),
            );
            call(
                &mut db,
                amm_addr,
                encode_words(
                    "swap(address,uint256)",
                    &[word_addr(a), word_u256(U256::from(500u64))],
                ),
            );
        };
        let gt = time_it(REPS, || run(&gum));
        let st = time_it(REPS, || run(&sol));
        print_time_row("amm (deploy+3 calls)", gt, st, REPS);
    }
    println!();
}

#[test]
fn fuzz_store_matches_solidity() {
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
        let sig = if rng.next_u64() % 2 == 0 {
            "set(uint256)"
        } else {
            "add(uint256)"
        };
        let data = encode(sig, &[v]);
        let gr = call(&mut gdb, gaddr, data.clone());
        let sr = call(&mut sdb, saddr, data);

        assert_eq!(
            gr.success, sr.success,
            "iter {}: success mismatch on {}({})",
            i, sig, v
        );
        assert_eq!(
            gr.output, sr.output,
            "iter {}: output mismatch on {}({})",
            i, sig, v
        );
        assert_eq!(
            storage(&mut gdb, gaddr, 0),
            storage(&mut sdb, saddr, 0),
            "iter {}: slot-0 mismatch after {}({})",
            i,
            sig,
            v
        );
    }
}

#[test]
fn fuzz_amm_matches_solidity() {
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

    let init = encode_words(
        "initialize(address,address)",
        &[word_addr(ga), word_addr(gb)],
    );
    assert!(call(&mut gdb, gamm, init.clone()).success);
    assert!(call(&mut sdb, samm, init).success);

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
            encode_words(
                "add_liquidity(uint256,uint256)",
                &[word_u256(a), word_u256(b)],
            )
        } else {
            let token = if rng.next_u64() % 2 == 0 { ga } else { gb };
            let amt = U256::from(rng.next_u64());
            encode_words("swap(address,uint256)", &[word_addr(token), word_u256(amt)])
        };
        let gr = call_from(&mut gdb, sender, gamm, data.clone());
        let sr = call_from(&mut sdb, sender, samm, data);

        assert_eq!(
            gr.success, sr.success,
            "iter {}: success mismatch (sender {:?})",
            i, sender
        );
        assert_eq!(gr.output, sr.output, "iter {}: output mismatch", i);
        assert_logs_match(&format!("iter {}", i), &gr, &sr);
        for slot in [2u64, 3, 4] {
            assert_eq!(
                storage(&mut gdb, gamm, slot),
                storage(&mut sdb, samm, slot),
                "iter {}: reserve/shares slot {} mismatch",
                i,
                slot
            );
        }

        for s in &senders {
            let mslot = mapping_slot(*s, 5);
            assert_eq!(
                storage_at(&mut gdb, gamm, mslot),
                storage_at(&mut sdb, samm, mslot),
                "iter {}: shares[{:?}] mismatch",
                i,
                s
            );
        }
    }
}

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

fn dyn_array_data_base(len_slot: u64) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(&U256::from(len_slot).to_be_bytes::<32>());
    k.finalize(&mut out);
    U256::from_be_bytes(out)
}

const GUM_DYN_ARRAY: &str = include_str!("fixtures/storage/dyn_array.gum");

const SOL_DYN_ARRAY: &str = include_str!("fixtures/storage/dyn_array.sol");

#[test]
fn dynamic_storage_array_matches_solidity() {
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
        assert_eq!(storage(gdb, ga, 0), storage(sdb, sa, 0), "length slot");
        for i in 0u64..4 {
            let s = data_base + U256::from(i);
            assert_eq!(
                storage_at(gdb, ga, s),
                storage_at(sdb, sa, s),
                "element {}",
                i
            );
        }
    };

    for (sig, words) in [
        ("push_val(uint256)", vec![word_u256(U256::from(10u64))]),
        ("push_val(uint256)", vec![word_u256(U256::from(20u64))]),
        ("push_val(uint256)", vec![word_u256(U256::from(30u64))]),
        (
            "set_at(uint256,uint256)",
            vec![word_u256(U256::from(1u64)), word_u256(U256::from(99u64))],
        ),
        ("pop_val()", vec![]),
    ] {
        let data = encode_words(sig, &words);
        let gr = call(&mut gdb, ga, data.clone());
        let sr = call(&mut sdb, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success", sig);
        assert_eq!(gr.output, sr.output, "{}: output", sig);
        check(&mut gdb, &mut sdb);
    }

    assert_eq!(
        U256::from_be_slice(&call(&mut gdb, ga, encode_words("len()", &[])).output),
        U256::from(2u64)
    );

    let g = call(
        &mut gdb,
        ga,
        encode_words("get(uint256)", &[word_u256(U256::from(5u64))]),
    );
    let s = call(
        &mut sdb,
        sa,
        encode_words("get(uint256)", &[word_u256(U256::from(5u64))]),
    );
    assert!(!g.success);
    assert_eq!(g.success, s.success, "OOB success");
    assert_eq!(g.output, s.output, "OOB Panic data");
}

const GUM_STORAGE_ARRAY: &str = include_str!("fixtures/storage/array.gum");

const SOL_STORAGE_ARRAY: &str = include_str!("fixtures/storage/array.sol");

#[test]
fn fixed_storage_array_matches_solidity() {
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
        let data = encode_words(
            "setit(uint256,uint256)",
            &[word_u256(U256::from(i)), word_u256(U256::from(v))],
        );
        assert!(call(&mut gdb, ga, data.clone()).success);
        assert!(call(&mut sdb, sa, data).success);
    }

    for slot in 0u64..=3 {
        assert_eq!(
            storage(&mut gdb, ga, slot),
            storage(&mut sdb, sa, slot),
            "slot {}",
            slot
        );
    }
    assert_eq!(storage(&mut gdb, ga, 3), U256::from(35u64), "total");
    let g = call(
        &mut gdb,
        ga,
        encode_words("getit(uint256)", &[word_u256(U256::from(2u64))]),
    );
    assert_eq!(U256::from_be_slice(&g.output), U256::from(20u64));

    for sig_words in [
        ("getit(uint256)", vec![word_u256(U256::from(3u64))]),
        (
            "setit(uint256,uint256)",
            vec![word_u256(U256::from(9u64)), word_u256(U256::from(1u64))],
        ),
    ] {
        let data = encode_words(sig_words.0, &sig_words.1);
        let gr = call(&mut gdb, ga, data.clone());
        let sr = call(&mut sdb, sa, data);
        assert!(!gr.success, "{}: gum should revert OOB", sig_words.0);
        assert_eq!(
            gr.success, sr.success,
            "{}: OOB success mismatch",
            sig_words.0
        );
        assert_eq!(
            gr.output, sr.output,
            "{}: OOB revert data mismatch (Panic 0x32)",
            sig_words.0
        );
    }
}

#[test]
fn once_function_reverts_on_second_call() {
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
    assert!(
        call(&mut db, a, init.clone()).success,
        "first initialize should succeed"
    );
    assert!(
        !call(&mut db, a, init).success,
        "second initialize must revert (once)"
    );
}

#[test]
fn erc721_matches_solidity() {
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
        (
            alice,
            "mint(address,uint256)",
            vec![word_addr(alice), word_u256(id)],
        ),
        (
            alice,
            "approve(address,uint256)",
            vec![word_addr(op), word_u256(id)],
        ),
        (
            alice,
            "setApprovalForAll(address,bool)",
            vec![word_addr(op), word_u256(U256::from(1u64))],
        ),
        (
            alice,
            "transferFrom(address,address,uint256)",
            vec![word_addr(alice), word_addr(bob), word_u256(id)],
        ),
        (alice, "balanceOf(address)", vec![word_addr(Address::ZERO)]),
        (
            alice,
            "ownerOf(uint256)",
            vec![word_u256(U256::from(999u64))],
        ),
        (
            op,
            "transferFrom(address,address,uint256)",
            vec![word_addr(bob), word_addr(alice), word_u256(id)],
        ),
        (
            alice,
            "mint(address,uint256)",
            vec![word_addr(alice), word_u256(id)],
        ),
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert_eq!(gr.output, sr.output, "{}: output/revert mismatch", sig);

        assert_eq!(
            storage_at(&mut gdb, ga, mapping_slot_uint(id, 2)),
            storage_at(&mut sdb, sa, mapping_slot_uint(id, 2)),
            "{}: owners[id]",
            sig
        );
        assert_eq!(
            storage_at(&mut gdb, ga, mapping_slot_uint(id, 4)),
            storage_at(&mut sdb, sa, mapping_slot_uint(id, 4)),
            "{}: approvals[id]",
            sig
        );

        for acct in [alice, bob] {
            let s = mapping_slot(acct, 3);
            assert_eq!(
                storage_at(&mut gdb, ga, s),
                storage_at(&mut sdb, sa, s),
                "{}: balance[{:?}]",
                sig,
                acct
            );
        }

        let s = nested_mapping_slot(alice, op, 5);
        assert_eq!(
            storage_at(&mut gdb, ga, s),
            storage_at(&mut sdb, sa, s),
            "{}: operator approval",
            sig
        );
    }

    assert_eq!(
        U256::from_be_slice(
            &call(
                &mut gdb,
                ga,
                encode_words("ownerOf(uint256)", &[word_u256(id)])
            )
            .output
        ),
        U256::from_be_slice(bob.as_slice())
    );
}

fn word_bytes4(id: u32) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[..4].copy_from_slice(&id.to_be_bytes());
    w
}

const SOL_STD_ERC20: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract ERC20 {
    string name_s;
    string symbol_s;
    uint256 total_supply;
    mapping(address => uint256) balances;
    mapping(address => mapping(address => uint256)) allowances;
    error ERC20InsufficientBalance(address sender, uint256 available, uint256 required);
    error ERC20InsufficientAllowance(address spender, uint256 available, uint256 required);
    error ERC20InvalidSender(address sender);
    error ERC20InvalidReceiver(address receiver);
    error ERC20InvalidApprover(address approver);
    error ERC20InvalidSpender(address spender);
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    constructor(string memory n, string memory s) { name_s = n; symbol_s = s; total_supply = 0; }
    function name() external view returns (string memory) { return name_s; }
    function symbol() external view returns (string memory) { return symbol_s; }
    function decimals() external pure returns (uint8) { return 18; }
    function totalSupply() external view returns (uint256) { return total_supply; }
    function balanceOf(address o) external view returns (uint256) { return balances[o]; }
    function allowance(address o, address s) external view returns (uint256) { return allowances[o][s]; }
    function transfer(address to, uint256 value) external returns (bool) {
        address sender = msg.sender;
        if (to == address(0)) revert ERC20InvalidReceiver(to);
        uint256 fb = balances[sender];
        if (fb < value) revert ERC20InsufficientBalance(sender, fb, value);
        balances[sender] = fb - value;
        balances[to] = balances[to] + value;
        emit Transfer(sender, to, value);
        return true;
    }
    function approve(address spender, uint256 value) external returns (bool) {
        address owner = msg.sender;
        if (spender == address(0)) revert ERC20InvalidSpender(spender);
        allowances[owner][spender] = value;
        emit Approval(owner, spender, value);
        return true;
    }
    function transferFrom(address from, address to, uint256 value) external returns (bool) {
        address spender = msg.sender;
        uint256 ca = allowances[from][spender];
        if (ca != type(uint256).max) {
            if (ca < value) revert ERC20InsufficientAllowance(spender, ca, value);
            allowances[from][spender] = ca - value;
        }
        if (to == address(0)) revert ERC20InvalidReceiver(to);
        uint256 fb = balances[from];
        if (fb < value) revert ERC20InsufficientBalance(from, fb, value);
        balances[from] = fb - value;
        balances[to] = balances[to] + value;
        emit Transfer(from, to, value);
        return true;
    }
    function _mint(address account, uint256 value) external {
        if (account == address(0)) revert ERC20InvalidReceiver(account);
        total_supply = total_supply + value;
        balances[account] = balances[account] + value;
        emit Transfer(address(0), account, value);
    }
    function _burn(address account, uint256 value) external {
        if (account == address(0)) revert ERC20InvalidSender(account);
        uint256 ab = balances[account];
        if (ab < value) revert ERC20InsufficientBalance(account, ab, value);
        balances[account] = ab - value;
        total_supply = total_supply - value;
        emit Transfer(account, address(0), value);
    }
}
";

#[test]
fn std_erc20_matches_a_solidity_twin() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    // Same (name, symbol) constructor args for both deployments.
    let ctor = encode_abi(
        "x(string,string)",
        &[Arg::Dyn(b"Tok".as_ref()), Arg::Dyn(b"TK".as_ref())],
    )[4..]
        .to_vec();

    let mut gum = gum_creation_bytecode(&read_repo_file("std/tokens/erc20.gum"), &solc, false);
    gum.extend_from_slice(&ctor);
    let mut sol = sol_creation_bytecode(SOL_STD_ERC20, &solc);
    sol.extend_from_slice(&ctor);

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let alice = deployer();
    let bob = Address::from([0x81u8; 20]);
    let carol = Address::from([0x82u8; 20]);
    let zero = Address::ZERO;

    let a = |x: Address| word_addr(x);
    let n = |v: u64| word_u256(U256::from(v));

    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (alice, "_mint(address,uint256)", vec![a(bob), n(1000)]),
        (alice, "_mint(address,uint256)", vec![a(alice), n(500)]),
        (alice, "totalSupply()", vec![]),
        (alice, "balanceOf(address)", vec![a(bob)]),
        (alice, "name()", vec![]),
        (alice, "decimals()", vec![]),
        (alice, "transfer(address,uint256)", vec![a(carol), n(200)]),
        // insufficient balance -> revert with matching custom-error data
        (alice, "transfer(address,uint256)", vec![a(carol), n(999999)]),
        // zero receiver -> revert
        (alice, "transfer(address,uint256)", vec![a(zero), n(1)]),
        (alice, "approve(address,uint256)", vec![a(bob), n(100)]),
        (alice, "allowance(address,address)", vec![a(alice), a(bob)]),
        (bob, "transferFrom(address,address,uint256)", vec![a(alice), a(carol), n(40)]),
        // allowance now 60; 999 exceeds it -> revert
        (bob, "transferFrom(address,address,uint256)", vec![a(alice), a(carol), n(999)]),
        // zero spender -> revert
        (alice, "approve(address,uint256)", vec![a(zero), n(1)]),
        (alice, "_burn(address,uint256)", vec![a(bob), n(100)]),
        // zero receiver mint -> revert
        (alice, "_mint(address,uint256)", vec![a(zero), n(1)]),
        (alice, "balanceOf(address)", vec![a(alice)]),
        (alice, "totalSupply()", vec![]),
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);

        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert_eq!(
            gr.output, sr.output,
            "{}: output/revert-data mismatch\n gum={:02x?}\n sol={:02x?}",
            sig, gr.output, sr.output
        );
        assert_eq!(gr.logs, sr.logs, "{}: event-log mismatch", sig);

        // total_supply is slot 2; balances is mapping at slot 3; allowances nested at slot 4.
        assert_eq!(
            storage_at(&mut gdb, ga, U256::from(2u64)),
            storage_at(&mut sdb, sa, U256::from(2u64)),
            "{}: total_supply",
            sig
        );
        for acct in [alice, bob, carol] {
            let s = mapping_slot(acct, 3);
            assert_eq!(
                storage_at(&mut gdb, ga, s),
                storage_at(&mut sdb, sa, s),
                "{}: balance[{:?}]",
                sig,
                acct
            );
        }
        let al = nested_mapping_slot(alice, bob, 4);
        assert_eq!(
            storage_at(&mut gdb, ga, al),
            storage_at(&mut sdb, sa, al),
            "{}: allowance[alice][bob]",
            sig
        );
    }
}

#[test]
fn abi_encode_matches_solidity() {
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
        (
            "e_static(uint256,address,bytes32)",
            vec![word_u256(U256::from(123u64)), word_addr(addr), c],
        ),
        (
            "p_static(uint256,address)",
            vec![word_u256(U256::from(123u64)), word_addr(addr)],
        ),
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

    let w = [0xABu8; 32];
    let out = call(&mut db, a, encode_words("echo32(bytes32)", &[w])).output;
    assert_eq!(&out[..32], &w, "b32 round-trips");

    let b4 = word_bytes4(0x01ffc9a7);
    let out = call(&mut db, a, encode_words("echo4(bytes4)", &[b4])).output;
    assert_eq!(&out[..32], &b4, "b4 round-trips left-aligned");

    let t = call(
        &mut db,
        a,
        encode_words("is165(bytes4)", &[word_bytes4(0x01ffc9a7)]),
    )
    .output;
    assert_eq!(
        U256::from_be_slice(&t),
        U256::from(1u8),
        "is165 true for 0x01ffc9a7"
    );
    let f = call(
        &mut db,
        a,
        encode_words("is165(bytes4)", &[word_bytes4(0xdeadbeef)]),
    )
    .output;
    assert_eq!(
        U256::from_be_slice(&f),
        U256::from(0u8),
        "is165 false otherwise"
    );
}

#[test]
fn erc721_supports_interface_matches_solidity() {
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

    for id in [
        0x01ffc9a7u32,
        0x80ac58cd,
        0x5b5e139f,
        0xffffffff,
        0x12345678,
    ] {
        let data = encode_words("supportsInterface(bytes4)", &[word_bytes4(id)]);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(
            g.success, s.success,
            "supportsInterface(0x{:08x}): success mismatch",
            id
        );
        assert_eq!(
            g.output, s.output,
            "supportsInterface(0x{:08x}): answer mismatch",
            id
        );
    }
}

#[test]
fn erc721_token_uri_matches_solidity() {
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
        let m = encode_words(
            "mint(address,uint256)",
            &[word_addr(alice), word_u256(U256::from(id))],
        );
        call(&mut gdb, ga, m.clone());
        call(&mut sdb, sa, m);
        let data = encode_words("tokenURI(uint256)", &[word_u256(U256::from(id))]);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(g.success, s.success, "tokenURI({}): success mismatch", id);
        assert_eq!(g.output, s.output, "tokenURI({}): uri mismatch", id);
    }

    let data = encode_words("tokenURI(uint256)", &[word_u256(U256::from(999u64))]);
    let g = call(&mut gdb, ga, data.clone());
    let s = call(&mut sdb, sa, data);
    assert!(
        !g.success && !s.success,
        "tokenURI(nonexistent) should revert on both"
    );
    assert_eq!(
        g.output, s.output,
        "tokenURI(nonexistent): revert data mismatch"
    );
}

#[test]
fn to_string_result_compares_as_a_string() {
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
    let yes = call(
        &mut db,
        a,
        encode_words("is7(uint256)", &[word_u256(U256::from(7u64))]),
    )
    .output;
    assert_eq!(
        U256::from_be_slice(&yes),
        U256::from(1u8),
        "7.to_string() == \"7\""
    );
    let no = call(
        &mut db,
        a,
        encode_words("is7(uint256)", &[word_u256(U256::from(8u64))]),
    )
    .output;
    assert_eq!(
        U256::from_be_slice(&no),
        U256::from(0u8),
        "8.to_string() != \"7\""
    );
}

#[test]
fn uint_to_string_produces_decimal() {
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
        (
            U256::from(1u128) << 128,
            "340282366920938463463374607431768211456",
        ),
        (
            U256::MAX,
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
        ),
    ] {
        let out = call(
            &mut db,
            a,
            encode_words("stringify(uint256)", &[word_u256(n)]),
        )
        .output;
        assert_eq!(decode(&out), expect, "stringify({})", n);
    }
}

fn nested_mapping_slot(k1: Address, k2: Address, base: u8) -> U256 {
    use tiny_keccak::{Hasher, Keccak};
    let inner = mapping_slot(k1, base);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(k2.as_slice());
    buf[32..64].copy_from_slice(&inner.to_be_bytes::<32>());
    let mut kk = Keccak::v256();
    let mut out = [0u8; 32];
    kk.update(&buf);
    kk.finalize(&mut out);
    U256::from_be_bytes(out)
}

const GUM_NESTED_BRACKET: &str = include_str!("fixtures/abi/nested_bracket.gum");

const SOL_NESTED_BRACKET: &str = include_str!("fixtures/abi/nested_bracket.sol");

#[test]
fn nested_mapping_bracket_sugar_matches_solidity() {
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
        (
            "setv(address,address,uint256)",
            vec![word_addr(a), word_addr(b), word_u256(U256::from(7u64))],
        ),
        (
            "incv(address,address,uint256)",
            vec![word_addr(a), word_addr(b), word_u256(U256::from(5u64))],
        ),
    ] {
        let data = encode_words(sig, &words);
        let gr = call(&mut gdb, ga, data.clone());
        let sr = call(&mut sdb, sa, data);
        assert_eq!(gr.success, sr.success, "{}", sig);
        assert_eq!(
            storage_at(&mut gdb, ga, slot),
            storage_at(&mut sdb, sa, slot),
            "{}: nested slot",
            sig
        );
    }
    let g = call(
        &mut gdb,
        ga,
        encode_words("getv(address,address)", &[word_addr(a), word_addr(b)]),
    );
    assert_eq!(
        U256::from_be_slice(&g.output),
        U256::from(12u64),
        "7 + 5 via nested RMW"
    );
}

#[test]
fn erc20_with_allowances_matches_solidity() {
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

    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (
            owner,
            "init(uint256)",
            vec![word_u256(U256::from(1_000_000u64))],
        ),
        (
            owner,
            "approve(address,uint256)",
            vec![word_addr(spender), word_u256(U256::from(400u64))],
        ),
        (
            spender,
            "transferFrom(address,address,uint256)",
            vec![
                word_addr(owner),
                word_addr(recipient),
                word_u256(U256::from(300u64)),
            ],
        ),
        (
            owner,
            "transfer(address,uint256)",
            vec![word_addr(recipient), word_u256(U256::from(50u64))],
        ),
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert!(gr.success, "{} reverted on gum", sig);
        assert_eq!(gr.output, sr.output, "{}: return mismatch", sig);

        assert_eq!(
            storage(&mut gdb, ga, 2),
            storage(&mut sdb, sa, 2),
            "{}: total_supply",
            sig
        );

        let bl = mapping_slot(owner, 0);
        assert_eq!(
            storage_at(&mut gdb, ga, bl),
            storage_at(&mut sdb, sa, bl),
            "{}: balance[owner]",
            sig
        );
        let br = mapping_slot(recipient, 0);
        assert_eq!(
            storage_at(&mut gdb, ga, br),
            storage_at(&mut sdb, sa, br),
            "{}: balance[recipient]",
            sig
        );

        let al = nested_mapping_slot(owner, spender, 1);
        assert_eq!(
            storage_at(&mut gdb, ga, al),
            storage_at(&mut sdb, sa, al),
            "{}: allowance[owner][spender]",
            sig
        );
    }

    assert_eq!(
        storage_at(&mut gdb, ga, mapping_slot(recipient, 0)),
        U256::from(350u64),
        "recipient balance"
    );
    assert_eq!(
        storage_at(&mut gdb, ga, nested_mapping_slot(owner, spender, 1)),
        U256::from(100u64),
        "remaining allowance"
    );
}

const GUM_PACKED_STRUCT: &str = include_str!("fixtures/storage/packed_struct.gum");

const SOL_PACKED_STRUCT: &str = include_str!("fixtures/storage/packed_struct.sol");

#[test]
fn packed_struct_slot_layout_matches_solidity() {
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
        &[
            word_addr(k),
            word_u256(U256::from(0xAAAu64)),
            word_u256(U256::from(0xBBBu64)),
        ],
    );
    assert!(call(&mut gdb, ga, data.clone()).success);
    assert!(call(&mut sdb, sa, data).success);

    let slot = mapping_slot(k, 0);
    assert_eq!(
        storage_at(&mut gdb, ga, slot),
        storage_at(&mut sdb, sa, slot),
        "packed struct slot word differs from Solidity"
    );

    let lo = call(
        &mut gdb,
        ga,
        encode_words("getlo(address)", &[word_addr(k)]),
    );
    let hi = call(
        &mut gdb,
        ga,
        encode_words("gethi(address)", &[word_addr(k)]),
    );
    assert_eq!(U256::from_be_slice(&lo.output), U256::from(0xAAAu64), "lo");
    assert_eq!(U256::from_be_slice(&hi.output), U256::from(0xBBBu64), "hi");
}

#[test]
fn vault_struct_in_mapping_matches_solidity() {
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
        (
            a,
            "deposit(uint256,uint256)",
            vec![
                word_u256(U256::from(100u64)),
                word_u256(U256::from(5000u64)),
            ],
        ),
        (
            a,
            "deposit(uint256,uint256)",
            vec![word_u256(U256::from(50u64)), word_u256(U256::from(6000u64))],
        ),
        (
            b,
            "deposit(uint256,uint256)",
            vec![
                word_u256(U256::from(200u64)),
                word_u256(U256::from(7000u64)),
            ],
        ),
        (a, "withdraw(uint256)", vec![word_u256(U256::from(30u64))]),
        (a, "withdraw(uint256)", vec![word_u256(U256::from(999u64))]),
    ];

    for (caller, sig, words) in &steps {
        let data = encode_words(sig, words);
        let gr = call_from(&mut gdb, *caller, ga, data.clone());
        let sr = call_from(&mut sdb, *caller, sa, data);
        assert_eq!(gr.success, sr.success, "{}: success mismatch", sig);
        assert_eq!(gr.output, sr.output, "{}: output/revert mismatch", sig);
        assert_eq!(
            storage(&mut gdb, ga, 0),
            storage(&mut sdb, sa, 0),
            "{}: total",
            sig
        );

        for acct in [a, b] {
            let base = mapping_slot(acct, 1);
            assert_eq!(
                storage_at(&mut gdb, ga, base),
                storage_at(&mut sdb, sa, base),
                "{}: {:?}.amount",
                sig,
                acct
            );
            let since = base + U256::from(1u64);
            assert_eq!(
                storage_at(&mut gdb, ga, since),
                storage_at(&mut sdb, sa, since),
                "{}: {:?}.since",
                sig,
                acct
            );
        }
    }

    let ga_base = mapping_slot(a, 1);
    assert_eq!(
        storage_at(&mut gdb, ga, ga_base),
        U256::from(120u64),
        "a.amount"
    );
    assert_eq!(
        storage_at(&mut gdb, ga, ga_base + U256::from(1u64)),
        U256::from(6000u64),
        "a.since"
    );
    assert_eq!(storage(&mut gdb, ga, 0), U256::from(320u64), "total");
}

#[test]
fn fuzz_erc20_matches_solidity() {
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

    let init = encode_words("init(uint256)", &[word_u256(U256::from(1_000_000u64))]);
    assert!(call(&mut gdb, ga, init.clone()).success);
    assert!(call(&mut sdb, sa, init).success);

    let mut rng = Rng(0xe2c0_1234);
    for i in 0..250 {
        let caller = accts[(rng.next_u64() % 4) as usize];
        let a1 = accts[(rng.next_u64() % 4) as usize];
        let a2 = accts[(rng.next_u64() % 4) as usize];
        let amt = U256::from(rng.next_u64() % 400_000);
        let data = match rng.next_u64() % 3 {
            0 => encode_words(
                "transfer(address,uint256)",
                &[word_addr(a1), word_u256(amt)],
            ),
            1 => encode_words("approve(address,uint256)", &[word_addr(a1), word_u256(amt)]),
            _ => encode_words(
                "transferFrom(address,address,uint256)",
                &[word_addr(a1), word_addr(a2), word_u256(amt)],
            ),
        };
        let gr = call_from(&mut gdb, caller, ga, data.clone());
        let sr = call_from(&mut sdb, caller, sa, data);
        assert_eq!(gr.success, sr.success, "iter {}: success mismatch", i);
        assert_eq!(gr.output, sr.output, "iter {}: output/revert mismatch", i);
        assert_eq!(
            storage(&mut gdb, ga, 2),
            storage(&mut sdb, sa, 2),
            "iter {}: total_supply",
            i
        );
        for acct in accts {
            let s = mapping_slot(acct, 0);
            assert_eq!(
                storage_at(&mut gdb, ga, s),
                storage_at(&mut sdb, sa, s),
                "iter {}: balance[{:?}]",
                i,
                acct
            );
        }
        for owner in accts {
            for spender in accts {
                let s = nested_mapping_slot(owner, spender, 1);
                assert_eq!(
                    storage_at(&mut gdb, ga, s),
                    storage_at(&mut sdb, sa, s),
                    "iter {}: allowance[{:?}][{:?}]",
                    i,
                    owner,
                    spender
                );
            }
        }
    }
}

const MEM_STRESS: &str = include_str!("fixtures/misc/mem_stress.gum");

fn mem_stress_expected(rounds: u64) -> U256 {
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
        let r = call(
            &mut gdb,
            addr,
            encode("mem_stress(uint256)", &[U256::from(rounds)]),
        );
        assert!(r.success, "mem_stress reverted at rounds={}", rounds);
        let got = U256::from_be_slice(&r.output);
        assert_eq!(
            got,
            mem_stress_expected(rounds),
            "memory corruption at rounds={}",
            rounds
        );
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
"
    .trim();

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

    let calldata = hex::decode("6e78dd6d").unwrap();

    let gum_res = call(&mut gum_db, gum_addr, calldata.clone());
    let sol_res = call(&mut sol_db, sol_addr, calldata);

    assert!(!gum_res.success, "gum did not revert!");
    let gum_revert_data = gum_res.output;

    assert!(!sol_res.success, "solidity did not revert!");
    let sol_revert_data = sol_res.output;

    assert_eq!(
        gum_revert_data, sol_revert_data,
        "Gum custom error data must exactly match Solidity"
    );
}

#[test]
fn custom_error_with_dynamic_string_arg_matches_solidity() {
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
"
    .trim();

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

    assert!(
        !gr.success && !sr.success,
        "both must revert (gum={}, sol={})",
        gr.success,
        sr.success
    );
    assert_eq!(
        gr.output, sr.output,
        "gum dynamic custom-error data must match Solidity byte-for-byte"
    );
}

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

    Arr(&'a [U256]),
}

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

    for which in [0u64, 1u64] {
        let data = encode_abi(
            "pick(uint256,string,string)",
            &[
                Arg::Static(word_u256(U256::from(which))),
                Arg::Dyn(a),
                Arg::Dyn(b),
            ],
        );
        let gr = call(&mut gdb, gaddr, data.clone());
        let sr = call(&mut sdb, saddr, data);
        assert!(
            gr.success && sr.success,
            "pick({}) reverted (gum={}, sol={})",
            which,
            gr.success,
            sr.success
        );
        assert_eq!(
            gr.output, sr.output,
            "pick({}) output must match Solidity",
            which
        );
    }

    let data = encode_abi("total_len(string,string)", &[Arg::Dyn(a), Arg::Dyn(b)]);
    let gr = call(&mut gdb, gaddr, data.clone());
    let sr = call(&mut sdb, saddr, data);
    assert!(gr.success && sr.success, "total_len reverted");
    assert_eq!(gr.output, sr.output, "total_len output must match Solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(a.len() + b.len()),
        "total_len value"
    );
}

#[test]
fn constructor_decodes_args_and_initializes_storage() {
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
"
    .trim();

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

    assert_eq!(
        storage(&mut gum_db, gum_addr, 1),
        supply_val,
        "gum total_supply slot"
    );
    assert_eq!(
        storage(&mut gum_db, gum_addr, 0),
        U256::from_be_bytes(word_addr(deployer())),
        "gum owner slot = deployer",
    );

    for sig in ["supply()", "owner_of()"] {
        let g = call(&mut gum_db, gum_addr, selector(sig).to_vec());
        let s = call(&mut sol_db, sol_addr, selector(sig).to_vec());
        assert!(g.success && s.success, "{} call failed", sig);
        assert_eq!(g.output, s.output, "{} output must match Solidity", sig);
    }
}

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
    assert_eq!(
        g.output, s.output,
        "string literal return must match Solidity"
    );
    assert_eq!(g.output, abi_encode_string(b"hello, gum world"));
}

#[test]
fn fstring_return_is_valid_string() {
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
    assert_eq!(
        g.output,
        abi_encode_string(b"n=42!"),
        "f-string must encode as the concatenated text"
    );
}

#[test]
fn custom_error_with_string_literal_arg_matches_solidity() {
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
    assert_eq!(
        g.output, s.output,
        "string-literal error arg must match Solidity"
    );
}

const GUM_STR_OPS: &str = include_str!("fixtures/string/ops.gum");

const SOL_STR_OPS: &str = include_str!("fixtures/string/ops.sol");

#[test]
fn string_ops_match_solidity() {
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
        (
            b"0123456789012345678901234567890123456789",
            b"012345678901234567890123456789012345678X",
        ),
        (
            b"X123456789012345678901234567890123456789",
            b"0123456789012345678901234567890123456789",
        ),
        (b"abc", b"abcd"),
        (b"hello ", b"world"),
    ];

    for (a, b) in cases {
        for sig in [
            "cat(string,string)",
            "same(string,string)",
            "differs(string,string)",
        ] {
            let data = encode_abi(sig, &[Arg::Dyn(a), Arg::Dyn(b)]);
            let g = call(&mut gdb, gaddr, data.clone());
            let s = call(&mut sdb, saddr, data);
            assert_eq!(
                g.success, s.success,
                "{} success differs for {:?}/{:?}",
                sig, a, b
            );
            assert_eq!(
                g.output, s.output,
                "{} output differs for {:?}/{:?}",
                sig, a, b
            );
        }
    }

    for i in 0..(long_a.len() + 1) {
        let data = encode_abi(
            "at(string,uint256)",
            &[Arg::Dyn(long_a), Arg::Static(word_u256(U256::from(i)))],
        );
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert_eq!(
            g.success, s.success,
            "at({}) success differs (gum={}, sol={})",
            i, g.success, s.success
        );
        if g.success {
            assert_eq!(g.output, s.output, "at({}) output differs", i);
        }
    }

    let slices: &[(u64, u64)] = &[
        (0, 0),
        (0, 5),
        (5, 5),
        (3, 40),
        (0, 61),
        (10, 61),
        (5, 3),
        (0, 62),
        (61, 62),
    ];
    for (s0, e0) in slices {
        let data = encode_abi(
            "cut(string,uint256,uint256)",
            &[
                Arg::Dyn(long_a),
                Arg::Static(word_u256(U256::from(*s0))),
                Arg::Static(word_u256(U256::from(*e0))),
            ],
        );
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert_eq!(
            g.success, s.success,
            "cut({},{}) success differs (gum={}, sol={})",
            s0, e0, g.success, s.success
        );
        if g.success {
            assert_eq!(g.output, s.output, "cut({},{}) output differs", s0, e0);
        }
    }
}

#[test]
fn assert_message_forms_match_solidity() {
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
            assert_eq!(
                g.output, s.output,
                "{} x={} revert/return data differs",
                sig, x
            );
        }
    }
}

#[test]
fn dynamic_constructor_args_match_solidity() {
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

    for name in [
        b"".as_ref(),
        b"gum".as_ref(),
        b"a name longer than thirty-two bytes for padding".as_ref(),
    ] {
        let args = &encode_abi(
            "x(string,uint256)",
            &[Arg::Dyn(name), Arg::Static(word_u256(U256::from(7u64)))],
        )[4..];

        let mut gum_code = gum_creation_bytecode(gum_src, &solc, false);
        gum_code.extend_from_slice(args);
        let mut sol_code = sol_creation_bytecode(sol_src, &solc);
        sol_code.extend_from_slice(args);

        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let gaddr = deploy(&mut gdb, gum_code);
        let saddr = deploy(&mut sdb, sol_code);

        assert_eq!(
            storage(&mut gdb, gaddr, 0),
            U256::from(name.len()),
            "gum name_len for {:?}",
            name
        );
        assert_eq!(storage(&mut gdb, gaddr, 1), U256::from(7u64), "gum id");

        for sig in ["len_of()", "id_of()"] {
            let g = call(&mut gdb, gaddr, selector(sig).to_vec());
            let s = call(&mut sdb, saddr, selector(sig).to_vec());
            assert!(g.success && s.success, "{} failed for {:?}", sig, name);
            assert_eq!(
                g.output, s.output,
                "{} differs from Solidity for {:?}",
                sig, name
            );
        }
    }
}

const GUM_PAYABLE: &str = include_str!("fixtures/context/payable.gum");

const SOL_PAYABLE: &str = include_str!("fixtures/context/payable.sol");

#[test]
fn payable_accepts_eth_and_nonpayable_still_rejects_it() {
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

    let d = selector("deposit()").to_vec();
    let g = call_with_value(&mut gdb, deployer(), gaddr, d.clone(), wei);
    let s = call_with_value(&mut sdb, deployer(), saddr, d, wei);
    assert!(
        g.success && s.success,
        "payable deposit must accept ETH (gum={}, sol={})",
        g.success,
        s.success
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 0),
        wei,
        "gum: msg.value must reach storage"
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 0),
        storage(&mut sdb, saddr, 0),
        "deposit storage must match Solidity"
    );

    for sig in ["poke()", "total_of()"] {
        let g = call_with_value(&mut gdb, deployer(), gaddr, selector(sig).to_vec(), wei);
        let s = call_with_value(&mut sdb, deployer(), saddr, selector(sig).to_vec(), wei);
        assert!(!s.success, "sanity: Solidity {} must reject ETH", sig);
        assert!(
            !g.success,
            "{} is not payable and must reject ETH, but it succeeded",
            sig
        );
        assert_eq!(
            g.success, s.success,
            "{} value-call behavior must match Solidity",
            sig
        );
    }

    for sig in ["poke()", "total_of()"] {
        let g = call(&mut gdb, gaddr, selector(sig).to_vec());
        let s = call(&mut sdb, saddr, selector(sig).to_vec());
        assert!(g.success && s.success, "{} must succeed with no value", sig);
        assert_eq!(g.output, s.output, "{} output must match Solidity", sig);
    }
}

#[test]
fn bare_return_early_exits_void_function() {
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
            "storage after set_unless_zero({}) differs from Solidity",
            x
        );
    }

    assert_eq!(
        storage(&mut gdb, gaddr, 0),
        U256::from(7u64),
        "early return must skip the write"
    );
}

const GUM_REENTRANT: &str = include_str!("fixtures/try/reentrant.gum");

const GUM_GUARDED_RETURNING: &str = include_str!("fixtures/try/guarded_returning.gum");

const SOL_BATCHER: &str = include_str!("fixtures/deploy/batcher.sol");

#[test]
fn guard_releases_the_lock_on_a_value_returning_entry_point() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let vault = deploy(
        &mut db,
        gum_creation_bytecode(GUM_GUARDED_RETURNING, &solc, false),
    );
    let batcher = deploy(&mut db, sol_creation_bytecode(SOL_BATCHER, &solc));
    assert!(
        call(
            &mut db,
            batcher,
            encode_words("setTarget(address)", &[word_addr(vault)])
        )
        .success
    );

    let r = call(&mut db, batcher, selector("twice()").to_vec());
    assert!(
        r.success,
        "two sequential calls in one transaction must both succeed, the guard \
         must release its lock on return, not leak it for the rest of the tx"
    );
    assert_eq!(
        storage(&mut db, vault, 0),
        U256::from(2u64),
        "both calls should have bumped the counter"
    );
}

const GUM_STRUCT_ARR: &str = include_str!("fixtures/abi/struct_in_array.gum");

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
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());

    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_STRUCT_ARR, &solc, true));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_STRUCT_ARR, &solc));

    macro_rules! same_storage {
        ($ctx:expr) => {{
            for slot in 0..2u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot),
                    storage(&mut sdb, saddr, slot),
                    "field slot {} differs after {}",
                    slot,
                    $ctx
                );
            }

            let base = dyn_array_data_base(1);
            for i in 0..6u64 {
                let s = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, gaddr, s),
                    storage_at(&mut sdb, saddr, s),
                    "struct array data slot {} differs after {}",
                    i,
                    $ctx
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

    for (a, s) in [(7u64, 9u64), (11, 13), (17, 19)] {
        both!(
            "add(uint256,uint256)",
            &[word_u256(U256::from(a)), word_u256(U256::from(s))]
        );
    }
    same_storage!("three pushes");

    let n = both!("len()", &[]);
    assert_eq!(
        U256::from_be_slice(&n.output),
        U256::from(3u64),
        "length should be 3"
    );

    for (i, (a, s)) in [(0u64, (7u64, 9u64)), (1, (11, 13)), (2, (17, 19))] {
        let ga = both!("get_amount(uint256)", &[word_u256(U256::from(i))]);
        assert_eq!(
            U256::from_be_slice(&ga.output),
            U256::from(a),
            "stakes[{}].amount",
            i
        );
        let gs = both!("get_since(uint256)", &[word_u256(U256::from(i))]);
        assert_eq!(
            U256::from_be_slice(&gs.output),
            U256::from(s),
            "stakes[{}].since",
            i
        );
    }

    both!(
        "set_amount(uint256,uint256)",
        &[word_u256(U256::from(1u64)), word_u256(U256::from(99u64))]
    );
    same_storage!("overwriting stakes[1].amount");
    let gs = both!("get_since(uint256)", &[word_u256(U256::from(1u64))]);
    assert_eq!(
        U256::from_be_slice(&gs.output),
        U256::from(13u64),
        "neighbour field was clobbered"
    );

    let oob = both!("get_amount(uint256)", &[word_u256(U256::from(3u64))]);
    assert!(!oob.success, "index 3 of a 3-element array must revert");

    both!("drop()", &[]);
    same_storage!("pop");
    let n = both!("len()", &[]);
    assert_eq!(
        U256::from_be_slice(&n.output),
        U256::from(2u64),
        "length should be 2 after pop"
    );

    both!("drop()", &[]);
    both!("drop()", &[]);
    same_storage!("popping to empty");
    let empty = both!("drop()", &[]);
    assert!(!empty.success, "popping an empty array must revert");
}

const GUM_PACKED_ARR: &str = include_str!("fixtures/storage/packed_array.gum");

const SOL_PACKED_ARR: &str = include_str!("fixtures/storage/packed_array.sol");

#[test]
fn packed_storage_array_layout_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());

    let gaddr = deploy(&mut gdb, gum_creation_bytecode(GUM_PACKED_ARR, &solc, true));
    let saddr = deploy(&mut sdb, sol_creation_bytecode(SOL_PACKED_ARR, &solc));

    macro_rules! same_storage {
        ($ctx:expr) => {{
            for slot in 0..3u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot),
                    storage(&mut sdb, saddr, slot),
                    "field slot {} differs after {}",
                    slot,
                    $ctx
                );
            }
            let base = dyn_array_data_base(0);
            for i in 0..4u64 {
                let s = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, gaddr, s),
                    storage_at(&mut sdb, saddr, s),
                    "dynamic array data slot {} differs after {}",
                    i,
                    $ctx
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

    for (i, v) in [(0u64, 0xaau64), (1, 0xbb), (2, 0xcc), (3, 0xdd)] {
        both!(
            "setf(uint256,uint8)",
            &[word_u256(U256::from(i)), word_u256(U256::from(v))]
        );
        both!("getf(uint256)", &[word_u256(U256::from(i))]);
    }
    same_storage!("filling the fixed array");
    assert_ne!(
        storage(&mut gdb, gaddr, 1),
        U256::ZERO,
        "the fixed array should be in slot 1"
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 2),
        U256::ZERO,
        "the fixed array must not have spilled onto sentinel"
    );

    for v in 0..33u64 {
        both!("push(uint8)", &[word_u256(U256::from(v + 1))]);
    }
    same_storage!("33 pushes");
    both!("len()", &[]);
    both!("sum()", &[]);
    for i in 0..33u64 {
        both!("get(uint256)", &[word_u256(U256::from(i))]);
    }

    let r = both!("get(uint256)", &[word_u256(U256::from(33u64))]);
    assert!(!r.success, "index 33 of a 33-element array must revert");

    for _ in 0..3 {
        both!("pop()", &[]);
    }
    same_storage!("popping back across the slot boundary");
    both!("sum()", &[]);

    for _ in 0..30 {
        both!("pop()", &[]);
    }
    same_storage!("draining the array");
    let r = both!("pop()", &[]);
    assert!(!r.success, "popping an empty array must revert");
}

const GUM_DELETE: &str = include_str!("fixtures/storage/delete.gum");

const SOL_DELETE: &str = include_str!("fixtures/storage/delete.sol");

#[test]
fn delete_matches_solidity() {
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

    let mut watched: Vec<U256> = (0..8u64).map(U256::from).collect();
    for i in 0..4u64 {
        watched.push(dyn_array_data_base(1) + U256::from(i));
        watched.push(dyn_array_data_base(2) + U256::from(i));
    }
    watched.push(mapping_slot(who, 4));
    watched.push(mapping_slot(who, 5));
    watched.push(mapping_slot(who, 5) + U256::from(1u64));

    macro_rules! same_storage {
        ($ctx:expr) => {
            for slot in &watched {
                assert_eq!(
                    storage_at(&mut gdb, gaddr, *slot),
                    storage_at(&mut sdb, saddr, *slot),
                    "slot {} differs after {}",
                    slot,
                    $ctx
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

    let long_name = vec![b'q'; 70];
    let fill = encode_abi(
        "fill(address,string)",
        &[Arg::Static(word_addr(who)), Arg::Dyn(&long_name)],
    );
    both!("fill", fill);
    same_storage!("fill");

    assert_ne!(
        storage(&mut gdb, gaddr, 0),
        U256::ZERO,
        "fill should have written something"
    );
    assert_ne!(
        storage_at(&mut gdb, gaddr, dyn_array_data_base(1)),
        U256::ZERO,
        "the long name should occupy data slots"
    );

    both!("wipe", encode_words("wipe(address)", &[word_addr(who)]));
    same_storage!("wipe");
    both!("len()", selector("len()").to_vec());
    both!("packed_b()", selector("packed_b()").to_vec());

    for slot in &watched {
        let v = storage_at(&mut gdb, gaddr, *slot);
        if *slot == U256::from(6u64) {
            assert_eq!(
                v,
                U256::from(9u64) << 8,
                "deleting packed_a must not disturb packed_b"
            );
        } else {
            assert_eq!(v, U256::ZERO, "slot {} should be zeroed after delete", slot);
        }
    }
}

const GUM_STORAGE_VEC: &str = include_str!("fixtures/storage/vec.gum");

const SOL_STORAGE_VEC: &str = include_str!("fixtures/storage/vec.sol");

#[test]
fn storage_vec_matches_a_solidity_dynamic_array() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let gaddr = deploy(
        &mut gdb,
        gum_creation_bytecode(GUM_STORAGE_VEC, &solc, true),
    );
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
                    storage(&mut gdb, gaddr, slot),
                    storage(&mut sdb, saddr, slot),
                    "field slot {} differs after {}",
                    slot,
                    $ctx
                );
            }
            let base = dyn_array_data_base(0);
            for i in 0..6u64 {
                let s = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, gaddr, s),
                    storage_at(&mut sdb, saddr, s),
                    "element slot {} differs after {}",
                    i,
                    $ctx
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

    assert_eq!(
        call(
            &mut gdb,
            gaddr,
            encode_words("get(uint256)", &[word_u256(U256::from(2u64))])
        )
        .output,
        call(
            &mut gdb,
            gaddr,
            encode_words("at(uint256)", &[word_u256(U256::from(2u64))])
        )
        .output,
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

const GUM_INHERIT: &str = include_str!("fixtures/deploy/inherit.gum");

const SOL_INHERIT: &str = include_str!("fixtures/deploy/inherit.sol");

#[test]
fn inheritance_matches_solidity() {
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

    both!("credit(uint256)", &[word_u256(U256::from(30u64))]);
    both!("credit(uint256)", &[word_u256(U256::from(12u64))]);

    both!("claim()", &[]);

    both!("set_fee(uint256)", &[word_u256(U256::from(9u64))]);

    both!("total()", &[]);
    both!("owner()", &[]);
    both!("fee()", &[]);

    both!("cap_of()", &[]);

    for slot in 0..3u64 {
        assert_eq!(
            storage(&mut gdb, gaddr, slot),
            storage(&mut sdb, saddr, slot),
            "inherited storage slot {} differs",
            slot
        );
    }

    assert_eq!(
        storage(&mut gdb, gaddr, 0),
        U256::from(42u64),
        "Ledger.total belongs in slot 0"
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        U256::from_be_slice(deployer().as_slice()),
        "Owned.owner belongs in slot 1"
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 2),
        U256::from(9u64),
        "Bank.fee belongs in slot 2"
    );
}

const GUM_BUBBLE: &str = include_str!("fixtures/try/bubble.gum");

const SOL_REVERTING_TOKEN: &str = include_str!("fixtures/try/reverting_token.sol");

const SOL_BUBBLE_CALLER: &str = include_str!("fixtures/try/bubble_caller.sol");

#[test]
fn external_call_reverts_bubble_up_like_solidity() {
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
            &[
                word_addr(token),
                word_addr(to),
                word_u256(U256::from(amount)),
            ],
        );
        let g = call(&mut db, gum_caller, data.clone());
        let s = call(&mut db, sol_caller, data);
        assert!(
            !g.success && !s.success,
            "both callers should revert for {}",
            what
        );
        assert!(
            !g.output.is_empty(),
            "gum swallowed the revert data for {}",
            what
        );
        assert_eq!(
            g.output, s.output,
            "gum's bubbled revert data must match Solidity's for {}",
            what
        );
    }

    let data = encode_words(
        "send(address,address,uint256)",
        &[word_addr(token), word_addr(to), word_u256(U256::from(1u64))],
    );
    let out = call(&mut db, gum_caller, data).output;
    assert_eq!(
        &out[..4],
        &[0x08, 0xc3, 0x79, 0xa0],
        "expected an Error(string) selector"
    );
    assert!(
        String::from_utf8_lossy(&out).contains("ERC20: transfer amount exceeds balance"),
        "the token's own reason string must survive the call boundary, got {:?}",
        out
    );

    let data = encode_words(
        "send(address,address,uint256)",
        &[word_addr(token), word_addr(to), word_u256(U256::from(5u64))],
    );
    let g = call(&mut db, gum_caller, data);
    assert!(g.success, "a succeeding transfer must still succeed");
    assert_eq!(
        g.output,
        word_u256(U256::from(1u64)).to_vec(),
        "should return true"
    );
}

const GUM_RECEIVE: &str = include_str!("fixtures/context/receive.gum");

const SOL_RECEIVE: &str = include_str!("fixtures/context/receive.sol");

const GUM_NO_RECEIVE: &str = include_str!("fixtures/context/no_receive.gum");

#[test]
fn receive_and_fallback_match_solidity() {
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

    macro_rules! both_val {
        ($data:expr, $wei:expr, $what:expr) => {{
            let g = call_with_value(&mut gdb, deployer(), gaddr, $data, U256::from($wei as u64));
            let s = call_with_value(&mut sdb, deployer(), saddr, $data, U256::from($wei as u64));
            assert_eq!(g.success, s.success, "success differs for {}", $what);
            assert_eq!(g.output, s.output, "output differs for {}", $what);
            for slot in 0..2u64 {
                assert_eq!(
                    storage(&mut gdb, gaddr, slot),
                    storage(&mut sdb, saddr, slot),
                    "slot {} differs after {}",
                    slot,
                    $what
                );
            }
            g
        }};
    }

    let r = both_val!(vec![], 1_000, "a bare ETH send");
    assert!(r.success, "a plain ETH send must reach receive()");
    assert_eq!(
        storage(&mut gdb, gaddr, 0),
        U256::from(1_000u64),
        "receive should have banked the ETH"
    );
    assert_eq!(
        gdb.basic(gaddr).unwrap().unwrap().balance,
        U256::from(1_000u64)
    );

    both_val!(vec![], 500, "a second bare ETH send");
    assert_eq!(storage(&mut gdb, gaddr, 0), U256::from(1_500u64));

    both_val!(vec![], 0, "an empty call with no value");

    both_val!(vec![0xde, 0xad, 0xbe, 0xef], 0, "an unmatched selector");
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        U256::from(1u64),
        "fallback should have run"
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 0),
        U256::from(1_500u64),
        "receive must not have run"
    );

    both_val!(vec![0x01, 0x02], 0, "two stray calldata bytes");
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        U256::from(2u64),
        "short calldata should reach fallback"
    );

    let r = both_val!(selector("total()").to_vec(), 0, "total()");
    assert!(r.success);
    assert_eq!(r.output, word_u256(U256::from(1_500u64)).to_vec());

    let mut ndb: Db = CacheDB::new(EmptyDB::default());
    let naddr = deploy(
        &mut ndb,
        gum_creation_bytecode(GUM_NO_RECEIVE, &solc, false),
    );
    let r = call_with_value(&mut ndb, deployer(), naddr, vec![], U256::from(10u64));
    assert!(
        !r.success,
        "a contract with no receive must reject a plain ETH send"
    );
    assert_eq!(
        ndb.basic(naddr)
            .unwrap()
            .map(|a| a.balance)
            .unwrap_or_default(),
        U256::ZERO
    );
}

#[test]
fn a_payable_receive_does_not_disarm_the_guard_on_other_functions() {
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
    let r = call_with_value(
        &mut db,
        deployer(),
        addr,
        selector("poke()").to_vec(),
        U256::from(5u64),
    );
    assert!(
        !r.success,
        "poke() is not payable and must still reject ETH even though receive() is payable"
    );
}

const GUM_FACTORY: &str = include_str!("fixtures/deploy/factory.gum");

const SOL_CHILD: &str = include_str!("fixtures/deploy/child.sol");

const SOL_BAD_CHILD: &str = include_str!("fixtures/deploy/bad_child.sol");

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

    let r = call(
        &mut db,
        factory,
        encode_abi("deploy(bytes)", &[Arg::Dyn(&child_code)]),
    );
    assert!(r.success, "deploy failed: {:?}", r.output);
    let child = addr_from_word(&r.output);
    assert_ne!(child, Address::ZERO, "should have returned a real address");

    let info = db
        .basic(child)
        .unwrap()
        .expect("child account should exist");
    assert!(
        info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "child should have runtime code"
    );

    let g = call(&mut db, child, selector("get()").to_vec());
    assert!(g.success, "child call failed");
    assert_eq!(
        g.output,
        word_u256(U256::from(42u64)).to_vec(),
        "the child's constructor should have run"
    );
    assert_eq!(
        storage(&mut db, factory, 0),
        U256::from(1u64),
        "the factory should have counted the deploy"
    );

    let salt = U256::from(0xcafeu64);
    let expected = create2_address(factory, salt, &child_code);

    let p = call(
        &mut db,
        factory,
        encode_abi(
            "predict(bytes,uint256)",
            &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt))],
        ),
    );
    assert!(p.success, "predict failed");
    assert_eq!(
        addr_from_word(&p.output),
        expected,
        "create2_address must match EIP-1014"
    );
    assert!(
        db.basic(expected)
            .unwrap()
            .map(|a| a.code.map(|c| c.is_empty()).unwrap_or(true))
            .unwrap_or(true),
        "nothing should be deployed at the predicted address yet"
    );

    let r = call(
        &mut db,
        factory,
        encode_abi(
            "deploy2(bytes,uint256)",
            &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt))],
        ),
    );
    assert!(r.success, "deploy2 failed: {:?}", r.output);
    assert_eq!(
        addr_from_word(&r.output),
        expected,
        "create2 must land on the predicted address"
    );
    let g = call(&mut db, expected, selector("get()").to_vec());
    assert!(
        g.success && g.output == word_u256(U256::from(42u64)).to_vec(),
        "the create2'd child should work"
    );

    let r = call(
        &mut db,
        factory,
        encode_abi(
            "deploy2(bytes,uint256)",
            &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt))],
        ),
    );
    assert!(
        !r.success,
        "redeploying at the same create2 address must revert, not return address 0"
    );

    let salt2 = U256::from(0xbeefu64);
    let r = call(
        &mut db,
        factory,
        encode_abi(
            "deploy2(bytes,uint256)",
            &[Arg::Dyn(&child_code), Arg::Static(word_u256(salt2))],
        ),
    );
    assert!(r.success, "a fresh salt should deploy");
    assert_eq!(
        addr_from_word(&r.output),
        create2_address(factory, salt2, &child_code)
    );

    let bad = sol_creation_bytecode(SOL_BAD_CHILD, &solc);
    let r = call(
        &mut db,
        factory,
        encode_abi("deploy(bytes)", &[Arg::Dyn(&bad)]),
    );
    assert!(
        !r.success,
        "a reverting child constructor must fail the deploy"
    );
    assert!(
        String::from_utf8_lossy(&r.output).contains("child ctor failed"),
        "the child constructor's own revert reason must bubble up, got {:?}",
        r.output
    );
}

#[test]
fn factory_can_fund_the_contract_it_deploys() {
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
    assert_eq!(
        db.basic(child).unwrap().unwrap().balance,
        wei,
        "the ETH should have gone to the child"
    );
    assert_eq!(
        db.basic(factory).unwrap().unwrap().balance,
        U256::ZERO,
        "the factory should not have kept it"
    );
}

const GUM_NEW_CONTRACT: &str = include_str!("fixtures/deploy/new_contract.gum");

const SOL_NEW_CONTRACT: &str = include_str!("fixtures/deploy/new_contract.sol");

fn deploy_named(db: &mut Db, solc: &Path, src: &str, name: &str) -> Address {
    deploy(db, gum_creation_bytecode_for(src, solc, false, name))
}

#[test]
fn new_contract_deploys_a_real_child() {
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
    let sfac = deploy(
        &mut sdb,
        sol_creation_bytecode_for(SOL_NEW_CONTRACT, &solc, "Factory"),
    );

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

    let info = gdb.basic(child).unwrap().expect("child should exist");
    assert!(
        info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "the deployed child must have runtime code"
    );

    let g = call(&mut gdb, child, selector("get()").to_vec());
    assert!(
        g.success && g.output == word_u256(U256::from(42u64)).to_vec(),
        "constructor arg should be stored"
    );
    let p = call(&mut gdb, child, selector("parent_of()").to_vec());
    assert_eq!(
        addr_from_word(&p.output),
        gfac,
        "the factory should be the child's msg.sender"
    );

    both!("count()", &[]);
    assert_eq!(storage(&mut gdb, gfac, 0), U256::from(1u64));
    assert_eq!(
        addr_from_word(&call(&mut gdb, gfac, selector("last()").to_vec()).output),
        child
    );

    let r2 = both!("make(uint256)", &[word_u256(U256::from(7u64))]);
    let child2 = addr_from_word(&r2.output);
    assert_ne!(
        child2, child,
        "a second make() must deploy a distinct contract"
    );
    let g = call(&mut gdb, child2, selector("get()").to_vec());
    assert_eq!(
        g.output,
        word_u256(U256::from(7u64)).to_vec(),
        "the second child gets its own arg"
    );

    let g = call(&mut gdb, child, selector("get()").to_vec());
    assert_eq!(
        g.output,
        word_u256(U256::from(42u64)).to_vec(),
        "the first child must be unaffected"
    );

    let s = call(
        &mut sdb,
        sfac,
        encode_words("make(uint256)", &[word_u256(U256::from(1u64))]),
    );
    assert!(s.success);
}

const GUM_NEW_DYN: &str = include_str!("fixtures/deploy/new_dyn.gum");

const SOL_NEW_DYN: &str = include_str!("fixtures/deploy/new_dyn.sol");

#[test]
fn new_contract_passes_dynamic_constructor_args() {
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
    let sdep = deploy(
        &mut sdb,
        sol_creation_bytecode_for(SOL_NEW_DYN, &solc, "Deployer"),
    );

    let cases: &[(&[u8], u64, &[u8])] = &[
        (b"Gum Token", 1_000_000, b"GUM"),
        (b"", 0, b""),
        (&[b'x'; 31], 7, &[b'y'; 32]),
        (&[b'z'; 100], u64::MAX, b"Q"),
    ];

    for (name, supply, sym) in cases {
        let data = encode_abi(
            "make(string,uint256,string)",
            &[
                Arg::Dyn(name),
                Arg::Static(word_u256(U256::from(*supply))),
                Arg::Dyn(sym),
            ],
        );
        let g = call(&mut gdb, gdep, data.clone());
        let s = call(&mut sdb, sdep, data);
        assert!(
            g.success && s.success,
            "make failed for name len {}: {:?}",
            name.len(),
            g.output
        );

        let gchild = addr_from_word(&g.output);
        let schild = addr_from_word(&s.output);

        for sig in ["name()", "symbol()", "supply()"] {
            let gr = call(&mut gdb, gchild, selector(sig).to_vec());
            let sr = call(&mut sdb, schild, selector(sig).to_vec());
            assert!(gr.success, "{} failed on the deployed child", sig);
            assert_eq!(
                gr.output,
                sr.output,
                "{} differs for name len {}",
                sig,
                name.len()
            );
        }

        for slot in 0..3u64 {
            assert_eq!(
                storage(&mut gdb, gchild, slot),
                storage(&mut sdb, schild, slot),
                "child slot {} differs for name len {}",
                slot,
                name.len()
            );
        }
    }

    let data = encode_abi(
        "make(string,uint256,string)",
        &[
            Arg::Dyn(b"Gum Token"),
            Arg::Static(word_u256(U256::from(5u64))),
            Arg::Dyn(b"GUM"),
        ],
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
        return Child.new(x)
";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let fac = deploy_named(&mut db, &solc, src, "Factory");

    assert!(
        call(
            &mut db,
            fac,
            encode_words("make(uint256)", &[word_u256(U256::from(5u64))])
        )
        .success
    );

    let r = call(
        &mut db,
        fac,
        encode_words("make(uint256)", &[word_u256(U256::ZERO)]),
    );
    assert!(
        !r.success,
        "a reverting child constructor must fail the deploy, not return address 0"
    );
    assert!(
        String::from_utf8_lossy(&r.output).contains("child: x must be positive"),
        "the child constructor's own reason must bubble up, got {:?}",
        r.output
    );
}

#[test]
fn a_deployment_cycle_is_rejected() {
    let src = "\
contract A:
    u256 x

    export fn make() -> Account:
        return B.new()

contract B:
    u256 y

    export fn make() -> Account:
        return A.new()
";
    let (ok, output) = run_gumc_exec(src);
    assert!(!ok, "expected a compile failure, got:\n{}", output);
    assert!(
        output.contains("Deployment cycle"),
        "expected a deployment-cycle diagnostic, got:\n{}",
        output
    );
}

const GUM_ARR_ABI: &str = include_str!("fixtures/abi/array.gum");

const SOL_ARR_ABI: &str = include_str!("fixtures/abi/array.sol");

#[test]
fn array_abi_args_and_returns_match_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());

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

        let w8 = words(&case.iter().map(|x| x % 256).collect::<Vec<_>>());
        both!("sum8(uint8[])", &[Arg::Arr(&w8)]);
        both!("echo8(uint8[])", &[Arg::Arr(&w8)]);

        if !case.is_empty() {
            both!(
                "at(uint256[],uint256)",
                &[
                    Arg::Arr(&w),
                    Arg::Static(word_u256(U256::from(case.len() as u64 - 1)))
                ]
            );
        }
        let r = both!(
            "at(uint256[],uint256)",
            &[
                Arg::Arr(&w),
                Arg::Static(word_u256(U256::from(case.len() as u64)))
            ]
        );
        assert!(
            !r.success,
            "index {} of a {}-element array must revert",
            case.len(),
            case.len()
        );
    }

    let a = words(&[10, 20, 30]);
    let b = words(&[1, 2]);
    let r = both!(
        "two(uint256[],uint256,uint8[])",
        &[
            Arg::Arr(&a),
            Arg::Static(word_u256(U256::from(5u64))),
            Arg::Arr(&b)
        ]
    );
    assert!(r.success);
    assert_eq!(
        r.output,
        word_u256(U256::from(68u64)).to_vec(),
        "10+20+30+5+1+2"
    );

    let w = words(&[1, 2, 3]);
    let r = call(&mut gdb, g, encode_abi("sum(uint256[])", &[Arg::Arr(&w)]));
    assert_eq!(
        r.output,
        word_u256(U256::from(6u64)).to_vec(),
        "sum([1,2,3]) should be 6"
    );
}

const GUM_NEW_ARR: &str = include_str!("fixtures/storage/new_array.gum");

const SOL_NEW_ARR: &str = include_str!("fixtures/storage/new_array.sol");

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
    let sm = deploy(
        &mut sdb,
        sol_creation_bytecode_for(SOL_NEW_ARR, &solc, "Maker"),
    );

    for case in [
        vec![],
        vec![5u64],
        vec![1, 2, 3, 4],
        (0..40u64).collect::<Vec<_>>(),
    ] {
        let w: Vec<U256> = case.iter().map(|x| U256::from(*x)).collect();
        let data = encode_abi(
            "make(uint256[],uint256)",
            &[Arg::Arr(&w), Arg::Static(word_u256(U256::from(100u64)))],
        );
        let gr = call(&mut gdb, gm, data.clone());
        let sr = call(&mut sdb, sm, data);
        assert!(
            gr.success && sr.success,
            "make failed for len {}: {:?}",
            case.len(),
            gr.output
        );

        let gchild = addr_from_word(&gr.output);
        let schild = addr_from_word(&sr.output);

        for slot in 0..2u64 {
            assert_eq!(
                storage(&mut gdb, gchild, slot),
                storage(&mut sdb, schild, slot),
                "child slot {} differs for array len {}",
                slot,
                case.len()
            );
        }
        let expected: u64 = 100 + case.iter().sum::<u64>();
        assert_eq!(
            storage(&mut gdb, gchild, 0),
            U256::from(expected),
            "total for len {}",
            case.len()
        );
        assert_eq!(
            storage(&mut gdb, gchild, 1),
            U256::from(case.len() as u64),
            "count"
        );
    }
}

const GUM_FARR_ABI: &str = include_str!("fixtures/abi/fixed_array.gum");

const SOL_FARR_ABI: &str = include_str!("fixtures/abi/fixed_array.sol");

#[test]
fn fixed_array_abi_matches_solidity() {
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

    let w = |v: u64| word_u256(U256::from(v));
    let r = both!("sum3(uint256[3])", &[w(10), w(20), w(30)]);
    assert!(r.success);
    assert_eq!(r.output, word_u256(U256::from(60u64)).to_vec(), "10+20+30");

    both!("echo3(uint256[3])", &[w(10), w(20), w(30)]);
    both!("sum3_8(uint8[3])", &[w(1), w(2), w(3)]);
    both!("echo3_8(uint8[3])", &[w(1), w(2), w(3)]);
    both!(
        "mixed(uint256,uint8[3],uint256)",
        &[w(100), w(1), w(2), w(3), w(200)]
    );

    let r = both!("sum3_8(uint8[3])", &[w(7), w(8), w(9)]);
    assert_eq!(r.output, word_u256(U256::from(24u64)).to_vec(), "7+8+9");
    let r = both!(
        "mixed(uint256,uint8[3],uint256)",
        &[w(100), w(1), w(2), w(3), w(200)]
    );
    assert_eq!(
        r.output,
        word_u256(U256::from(306u64)).to_vec(),
        "100+200+1+2+3"
    );

    let r = call(&mut gdb, g, encode_words("sum3(uint256[3])", &[w(1), w(2)]));
    assert!(
        !r.success,
        "a [u256; 3] argument with only 2 words must revert"
    );
}

const GUM_TRANSIENT: &str = include_str!("fixtures/storage/transient.gum");

#[test]
fn transient_fields_hold_within_a_transaction() {
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
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(GUM_TRANSIENT, &solc, false));
    let who = Address::from([0x33u8; 20]);

    let data = encode_abi(
        "set_all(address,string)",
        &[Arg::Static(word_addr(who)), Arg::Dyn(b"hello")],
    );
    assert!(call(&mut db, addr, data).success, "set_all failed");

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

    let r = call(&mut db, addr, selector("tstr_of()").to_vec());
    assert!(r.success);
    assert_eq!(
        r.output,
        encode_abi_return_string(b""),
        "a transient string must be empty in a later transaction"
    );

    let r = call(&mut db, addr, selector("read_a()").to_vec());
    assert_eq!(
        r.output,
        word_u256(U256::from(11u64)).to_vec(),
        "the persistent scalar must survive"
    );
    let r = call(&mut db, addr, selector("parr_len()").to_vec());
    assert_eq!(
        r.output,
        word_u256(U256::from(1u64)).to_vec(),
        "the persistent array must survive"
    );
    let r = call(
        &mut db,
        addr,
        encode_words("pmap_of(address)", &[word_addr(who)]),
    );
    assert_eq!(
        r.output,
        word_u256(U256::from(1001u64)).to_vec(),
        "the persistent map must survive"
    );
    let r = call(&mut db, addr, selector("pstr_of()").to_vec());
    assert_eq!(
        r.output,
        encode_abi_return_string(b"hello"),
        "the persistent string must survive"
    );
}

#[test]
fn transient_and_persistent_slots_do_not_collide() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(GUM_TRANSIENT, &solc, false));
    let who = Address::from([0x33u8; 20]);

    let data = encode_abi(
        "set_all(address,string)",
        &[Arg::Static(word_addr(who)), Arg::Dyn(b"hi")],
    );
    assert!(call(&mut db, addr, data).success);

    let persistent: Vec<U256> = (0..4u64).map(|s| storage(&mut db, addr, s)).collect();
    for v in [22u64, 202, 2002] {
        assert!(
            !persistent.contains(&U256::from(v)),
            "a transient value ({}) leaked into persistent storage: {:?}",
            v,
            persistent
        );
    }

    assert!(
        persistent.contains(&U256::from(11u64)),
        "the persistent scalar should be in storage: {:?}",
        persistent
    );
}

const SOL_ATTACKER: &str = include_str!("fixtures/try/attacker.sol");

#[test]
fn reentrancy_guard_blocks_a_real_attack_and_unsafe_opts_out() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };

    {
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let vault = deploy(
            &mut db,
            gum_creation_bytecode(&GUM_REENTRANT.replace("{MOD}", ""), &solc, false),
        );
        let atk = deploy(&mut db, sol_creation_bytecode(SOL_ATTACKER, &solc));
        let set = encode_words("setTarget(address)", &[word_addr(vault)]);
        assert!(call(&mut db, atk, set).success, "setTarget failed");

        let r = call(
            &mut db,
            vault,
            encode_words("poke(address)", &[word_addr(atk)]),
        );
        assert!(
            !r.success,
            "reentrancy guard must block the attack, but poke() succeeded"
        );
        assert_eq!(
            storage(&mut db, vault, 0),
            U256::ZERO,
            "guarded: reverted call must leave counter at 0"
        );
    }

    {
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let vault = deploy(
            &mut db,
            gum_creation_bytecode(&GUM_REENTRANT.replace("{MOD}", "unsafe "), &solc, false),
        );
        let atk = deploy(&mut db, sol_creation_bytecode(SOL_ATTACKER, &solc));
        let set = encode_words("setTarget(address)", &[word_addr(vault)]);
        assert!(call(&mut db, atk, set).success, "setTarget failed");

        let r = call(
            &mut db,
            vault,
            encode_words("poke(address)", &[word_addr(atk)]),
        );
        assert!(
            r.success,
            "unsafe fn should permit reentrancy, but the call failed"
        );
        assert_eq!(
            storage(&mut db, vault, 0),
            U256::from(2u64),
            "unsafe: the re-entrant call should have bumped counter twice"
        );
    }
}

#[test]
fn reentrancy_lock_does_not_leak_across_calls_in_one_transaction() {
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

    for i in 1..=3u64 {
        let r = call(&mut db, addr, selector("bump()").to_vec());
        assert!(
            r.success,
            "call {} must succeed, the lock must not survive the previous call",
            i
        );
        assert_eq!(
            storage(&mut db, addr, 0),
            U256::from(i),
            "counter after call {}",
            i
        );
    }
}

#[test]
fn account_pay_transfers_eth() {
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
    let r = call_with_value(
        &mut db,
        deployer(),
        vault,
        selector("deposit()").to_vec(),
        wei,
    );
    assert!(r.success, "payable deposit failed");
    assert_eq!(
        db.basic(vault).unwrap().unwrap().balance,
        wei,
        "vault should hold the deposit"
    );

    let payee = Address::from([0x77u8; 20]);
    let before = db
        .basic(payee)
        .unwrap()
        .map(|a| a.balance)
        .unwrap_or_default();
    let data = encode_words(
        "withdraw(address,uint256)",
        &[word_addr(payee), word_u256(U256::from(2_000u64))],
    );
    assert!(call(&mut db, vault, data).success, "withdraw failed");

    let after = db.basic(payee).unwrap().unwrap().balance;
    assert_eq!(
        after - before,
        U256::from(2_000u64),
        "pay() must actually transfer the ETH"
    );
    assert_eq!(
        db.basic(vault).unwrap().unwrap().balance,
        U256::from(3_000u64),
        "vault balance must drop"
    );
    assert_eq!(
        storage(&mut db, vault, 0),
        U256::from(3_000u64),
        "accounting slot must match"
    );
}

#[test]
fn account_transfer_sends_eth_and_reverts_on_failure() {
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
    assert!(
        call_with_value(
            &mut db,
            deployer(),
            vault,
            selector("deposit()").to_vec(),
            wei
        )
        .success
    );

    let payee = Address::from([0x77u8; 20]);
    let data = encode_words(
        "send(address,uint256)",
        &[word_addr(payee), word_u256(U256::from(2_000u64))],
    );
    assert!(
        call(&mut db, vault, data).success,
        "transfer to a plain EOA should succeed"
    );
    assert_eq!(
        db.basic(payee).unwrap().unwrap().balance,
        U256::from(2_000u64),
        "transfer() must actually move the ETH"
    );
    assert_eq!(
        db.basic(vault).unwrap().unwrap().balance,
        U256::from(3_000u64),
        "vault balance must drop"
    );
    assert_eq!(storage(&mut db, vault, 0), U256::from(3_000u64));

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

    let data = encode_words(
        "send(address,uint256)",
        &[word_addr(rejector), word_u256(U256::from(1_000u64))],
    );
    let r = call(&mut db, vault, data);
    assert!(
        !r.success,
        "transfer() to a rejecting recipient must revert, not silently continue"
    );
    assert_eq!(
        r.output,
        vec![0xde, 0xad, 0xbe, 0xef],
        "the recipient's own revert data must be bubbled up"
    );

    assert_eq!(
        storage(&mut db, vault, 0),
        U256::from(3_000u64),
        "failed transfer must not have debited the total"
    );
    assert_eq!(
        db.basic(vault).unwrap().unwrap().balance,
        U256::from(3_000u64),
        "vault must still hold the ETH"
    );
}

const GUM_P256: &str = include_str!("fixtures/crypto/p256.gum");

#[test]
fn p256_verify_accepts_a_real_signature_and_rejects_tampering() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    use p256::ecdsa::{Signature, SigningKey, signature::hazmat::PrehashSigner};

    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut gdb, gum_creation_bytecode(GUM_P256, &solc, false));

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

    let good = call(&mut gdb, addr, encode(sig, &[h, r, s, qx, qy]));
    assert!(good.success, "verify_p256 reverted");
    assert_eq!(
        U256::from_be_slice(&good.output),
        U256::from(1u64),
        "a valid P-256 signature must verify (is the 0x100 precompile active?)"
    );

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
        assert_eq!(
            U256::from_be_slice(&bad.output),
            U256::ZERO,
            "{}: must NOT verify",
            what
        );
    }
}

#[test]
fn eip7702_delegated_to_reads_the_delegation_indicator() {
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

    let target = Address::from([0xabu8; 20]);
    let bc = Bytecode::new_eip7702(target);
    let delegated = Address::from([0x42u8; 20]);
    let mut info = AccountInfo::default();
    info.code_hash = bc.hash_slow();
    info.code = Some(bc);
    db.insert_account_info(delegated, info);

    let plain = Address::from([0x43u8; 20]);
    db.insert_account_info(plain, AccountInfo::default());

    let cases: &[(Address, Address, bool)] = &[
        (delegated, target, true),
        (plain, Address::ZERO, false),
        (addr, Address::ZERO, false),
    ];
    for (who, want, want_flag) in cases {
        let r = call(
            &mut db,
            addr,
            encode_words("deleg(address)", &[word_addr(*who)]),
        );
        assert!(r.success, "deleg() reverted for {:?}", who);
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from_be_bytes(word_addr(*want)),
            "delegated_to({:?}) wrong",
            who
        );
        let f = call(
            &mut db,
            addr,
            encode_words("is_deleg(address)", &[word_addr(*who)]),
        );
        assert!(f.success, "is_deleg() reverted");
        assert_eq!(
            U256::from_be_slice(&f.output) == U256::from(1u64),
            *want_flag,
            "is_delegated({:?}) wrong",
            who
        );
    }
}

const GUM_CRYPTO: &str = include_str!("fixtures/crypto/crypto.gum");

const SOL_CRYPTO: &str = include_str!("fixtures/crypto/crypto.sol");

#[test]
fn keccak256_and_ecrecover_match_solidity() {
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
        assert_eq!(
            g.output, s.output,
            "keccak256 differs from Solidity for {:?}",
            msg
        );
    }

    let sig = "rec(uint256,uint8,uint256,uint256)";
    let cases: &[[U256; 4]] = &[
        [
            U256::from_be_slice(
                &hex::decode("bb1a0f1b0e0b0d6b5a8b1c0f2c4a5e6d7c8b9a0f1e2d3c4b5a6978869504f3e2")
                    .unwrap(),
            ),
            U256::from(27u64),
            U256::from_be_slice(
                &hex::decode("6b8a3f2f1e0d9c8b7a695847362514039281706f5e4d3c2b1a09f8e7d6c5b4a3")
                    .unwrap(),
            ),
            U256::from_be_slice(
                &hex::decode("1c2d3e4f50617283940516273849506172839405162738495061728394051627")
                    .unwrap(),
            ),
        ],
        [U256::ZERO, U256::from(27u64), U256::ZERO, U256::ZERO],
        [
            U256::from(1u64),
            U256::from(99u64),
            U256::from(2u64),
            U256::from(3u64),
        ],
    ];
    for args in cases {
        let data = encode(sig, args);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert_eq!(g.success, s.success, "ecrecover success differs");
        assert_eq!(
            g.output, s.output,
            "ecrecover result differs from Solidity for {:?}",
            args
        );
    }
}

const GUM_SSTR: &str = include_str!("fixtures/string/storage.gum");

const SOL_SSTR: &str = include_str!("fixtures/string/storage.sol");

#[test]
fn storage_string_layout_matches_solidity() {
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

    let cases: &[&[u8]] = &[
        b"",
        b"a",
        b"Gum Token",
        &[b'x'; 31],
        &[b'y'; 32],
        &[b'z'; 33],
        &[b'w'; 100],
        b"back to short",
        b"",
    ];

    for name in cases {
        let data = encode_abi("set_name(string)", &[Arg::Dyn(name)]);
        let g = call(&mut gdb, gaddr, data.clone());
        let s = call(&mut sdb, saddr, data);
        assert!(
            g.success && s.success,
            "set_name failed for len {}",
            name.len()
        );

        assert_eq!(
            storage(&mut gdb, gaddr, 0),
            storage(&mut sdb, saddr, 0),
            "storage string header slot differs for len {}",
            name.len()
        );

        let base = dyn_array_data_base(0);
        for i in 0..5u64 {
            let slot = base + U256::from(i);
            assert_eq!(
                storage_at(&mut gdb, gaddr, slot),
                storage_at(&mut sdb, saddr, slot),
                "storage string data slot {} differs for len {}",
                i,
                name.len()
            );
        }

        let g = call(&mut gdb, gaddr, selector("name_of()").to_vec());
        let s = call(&mut sdb, saddr, selector("name_of()").to_vec());
        assert!(
            g.success && s.success,
            "name_of failed for len {}",
            name.len()
        );
        assert_eq!(
            g.output,
            s.output,
            "name_of round-trip differs for len {}",
            name.len()
        );
        assert_eq!(
            g.output,
            abi_encode_string(name),
            "name_of must return the value we set"
        );
    }

    let d = encode("set_supply(uint256)", &[U256::from(4242u64)]);
    assert!(call(&mut gdb, gaddr, d.clone()).success);
    assert!(call(&mut sdb, saddr, d).success);
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        U256::from(4242u64),
        "supply must live in its own slot"
    );
    assert_eq!(
        storage(&mut gdb, gaddr, 1),
        storage(&mut sdb, saddr, 1),
        "supply slot differs"
    );
}

#[test]
fn mapping_string_value_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/map/string.gum");
    let sol_src = include_str!("fixtures/map/string.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

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

    let cases: &[&[u8]] = &[
        b"",
        b"a",
        b"Alice",
        &[b'x'; 31],
        &[b'y'; 32],
        &[b'z'; 100],
        b"short again",
        b"",
    ];

    for name in cases {
        let data = encode_abi(
            "set(address,string)",
            &[Arg::Static(word_addr(alice)), Arg::Dyn(name)],
        );
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert!(g.success && s.success, "set failed for len {}", name.len());

        let vslot = mapping_slot(alice, 0);
        assert_eq!(
            storage_at(&mut gdb, ga, vslot),
            storage_at(&mut sdb, sa, vslot),
            "value/header slot differs for len {}",
            name.len()
        );

        let base = str_data_base(vslot);
        for i in 0..5u64 {
            let slot = base + U256::from(i);
            assert_eq!(
                storage_at(&mut gdb, ga, slot),
                storage_at(&mut sdb, sa, slot),
                "data slot {} differs for len {}",
                i,
                name.len()
            );
        }

        let rd = encode_abi("get(address)", &[Arg::Static(word_addr(alice))]);
        let g = call(&mut gdb, ga, rd.clone());
        let s = call(&mut sdb, sa, rd);
        assert!(g.success && s.success, "get failed for len {}", name.len());
        assert_eq!(
            g.output,
            s.output,
            "get round-trip differs for len {}",
            name.len()
        );
        assert_eq!(
            g.output,
            abi_encode_string(name),
            "get must return the value we set"
        );
    }

    let bob_name: &[u8] = b"Bob the builder, a long enough name to go long-form for sure yes";
    let data = encode_abi(
        "set(address,string)",
        &[Arg::Static(word_addr(bob)), Arg::Dyn(bob_name)],
    );
    assert!(call(&mut gdb, ga, data.clone()).success);
    assert!(call(&mut sdb, sa, data).success);
    for who in [alice, bob] {
        let vslot = mapping_slot(who, 0);
        assert_eq!(
            storage_at(&mut gdb, ga, vslot),
            storage_at(&mut sdb, sa, vslot),
            "value slot differs for {:?}",
            who
        );
    }

    let del = encode_abi("clear(address)", &[Arg::Static(word_addr(bob))]);
    assert!(call(&mut gdb, ga, del.clone()).success);
    assert!(call(&mut sdb, sa, del).success);
    let vslot = mapping_slot(bob, 0);
    assert_eq!(
        storage_at(&mut gdb, ga, vslot),
        U256::ZERO,
        "value slot not cleared"
    );
    let base = str_data_base(vslot);
    for i in 0..3u64 {
        let slot = base + U256::from(i);
        assert_eq!(
            storage_at(&mut gdb, ga, slot),
            storage_at(&mut sdb, sa, slot),
            "data slot {} not released like Solidity",
            i
        );
    }
}

#[test]
fn mapping_dynamic_array_value_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/map/dyn_array.gum");
    let sol_src = include_str!("fixtures/map/dyn_array.sol");

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

    let steps: Vec<(Address, &str, Vec<[u8; 32]>)> = vec![
        (
            alice,
            "add(address,uint256)",
            vec![word_u256(U256::from(10u64))],
        ),
        (
            alice,
            "add(address,uint256)",
            vec![word_u256(U256::from(20u64))],
        ),
        (
            alice,
            "add(address,uint256)",
            vec![word_u256(U256::from(30u64))],
        ),
        (
            bob,
            "add(address,uint256)",
            vec![word_u256(U256::from(99u64))],
        ),
        (
            alice,
            "set(address,uint256,uint256)",
            vec![
                word_addr(alice),
                word_u256(U256::from(1u64)),
                word_u256(U256::from(25u64)),
            ],
        ),
        (alice, "drop_last(address)", vec![]),
    ];

    for (caller, sig, tail) in &steps {
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
                "{}: length slot for {:?}",
                sig,
                who
            );
            let base = arr_base(vslot);
            for i in 0..4u64 {
                let slot = base + U256::from(i);
                assert_eq!(
                    storage_at(&mut gdb, ga, slot),
                    storage_at(&mut sdb, sa, slot),
                    "{}: element slot {} for {:?}",
                    sig,
                    i,
                    who
                );
            }

            let sz = call(
                &mut gdb,
                ga,
                encode_words("size(address)", &[word_addr(who)]),
            );
            let sz2 = call(
                &mut sdb,
                sa,
                encode_words("size(address)", &[word_addr(who)]),
            );
            assert_eq!(sz.output, sz2.output, "{}: size() for {:?}", sig, who);
            let n = U256::from_be_slice(&sz.output).to::<u64>();
            for i in 0..n {
                let gi = call(
                    &mut gdb,
                    ga,
                    encode_words(
                        "get(address,uint256)",
                        &[word_addr(who), word_u256(U256::from(i))],
                    ),
                );
                let si = call(
                    &mut sdb,
                    sa,
                    encode_words(
                        "get(address,uint256)",
                        &[word_addr(who), word_u256(U256::from(i))],
                    ),
                );
                assert_eq!(
                    gi.success, si.success,
                    "{}: get({}) success for {:?}",
                    sig, i, who
                );
                assert_eq!(gi.output, si.output, "{}: get({}) for {:?}", sig, i, who);
            }
        }
    }

    let del = encode_words("clear(address)", &[word_addr(alice)]);
    assert!(call(&mut gdb, ga, del.clone()).success);
    assert!(call(&mut sdb, sa, del).success);
    let vslot = mapping_slot(alice, 0);
    assert_eq!(
        storage_at(&mut gdb, ga, vslot),
        U256::ZERO,
        "length not cleared"
    );
    let base = arr_base(vslot);
    for i in 0..4u64 {
        let slot = base + U256::from(i);
        assert_eq!(
            storage_at(&mut gdb, ga, slot),
            storage_at(&mut sdb, sa, slot),
            "element slot {} not released like Solidity",
            i
        );
    }
}

#[test]
fn string_array_across_the_abi_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/string/array.gum");
    let sol_src = include_str!("fixtures/string/array.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let encode_str_arr = |sig: &str, items: &[&[u8]]| -> Vec<u8> {
        let mut data = selector(sig).to_vec();
        data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
        let n = items.len();
        let mut table = Vec::new();
        let mut tails = Vec::new();
        let mut cur = n * 32;
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
        data.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
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
        assert_eq!(
            g.success,
            s.success,
            "echo success mismatch for {} items",
            items.len()
        );
        assert!(g.success, "echo reverted for {} items", items.len());
        assert_eq!(
            g.output,
            s.output,
            "echo output differs for {} items",
            items.len()
        );

        let da = encode_str_arr("alen(string[])", items);
        let g = call(&mut gdb, ga, da.clone());
        let s = call(&mut sdb, sa, da);
        assert_eq!(
            g.output,
            s.output,
            "xs.length differs for {} items",
            items.len()
        );
        assert_eq!(
            U256::from_be_slice(&g.output),
            U256::from(items.len()),
            "xs.length wrong"
        );

        if !items.is_empty() {
            let dp = encode_str_arr("plen(string[])", items);
            let g = call(&mut gdb, ga, dp.clone());
            let s = call(&mut sdb, sa, dp);
            assert_eq!(g.output, s.output, "xs[0].length differs");
            assert_eq!(
                U256::from_be_slice(&g.output),
                U256::from(items[0].len()),
                "xs[0].length wrong"
            );

            let data = encode_str_arr("first(string[])", items);
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(g.success, s.success, "first success mismatch");
            assert!(g.success, "first reverted");
            assert_eq!(g.output, s.output, "first output differs");
            assert_eq!(
                g.output,
                abi_encode_string(items[0]),
                "first must return element 0"
            );
        }
    }
}

#[test]
fn dynamic_struct_across_the_abi_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/abi/dyn_struct.gum");
    let sol_src = include_str!("fixtures/abi/dyn_struct.sol");

    let gum = gum_creation_bytecode(gum_src, &solc, false);
    let sol = sol_creation_bytecode(sol_src, &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);

    let encode = |id: u64, name: &[u8]| -> Vec<u8> {
        let mut data = selector("echo((uint256,string))").to_vec();
        data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(id).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(64u64).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(name.len()).to_be_bytes::<32>());
        let mut nb = name.to_vec();
        let pad = (32 - (name.len() % 32)) % 32;
        nb.extend(std::iter::repeat(0u8).take(pad));
        data.extend_from_slice(&nb);
        data
    };

    let cases: &[(u64, &[u8])] = &[
        (0, b""),
        (7, b"Alice"),
        (42, &[b'x'; 31]),
        (100, &[b'y'; 32]),
        (999, &[b'z'; 80]),
    ];
    for (id, name) in cases {
        let data = encode(*id, name);
        let g = call(&mut gdb, ga, data.clone());
        let s = call(&mut sdb, sa, data);
        assert_eq!(
            g.success,
            s.success,
            "echo success for id {} len {}",
            id,
            name.len()
        );
        assert!(g.success, "echo reverted for id {} len {}", id, name.len());
        assert_eq!(
            g.output,
            s.output,
            "echo output differs for id {} len {}",
            id,
            name.len()
        );
    }
}

#[test]
fn dynamic_struct_through_an_interface_call_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let gum_src = include_str!("fixtures/abi/dyn_struct_iface.gum");
    let sol_src = include_str!("fixtures/abi/dyn_struct_iface.sol");

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
        data.extend_from_slice(&U256::from(96u64).to_be_bytes::<32>());
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
        assert_eq!(
            g.success,
            s.success,
            "call_it success for id {} len {}",
            id,
            name.len()
        );
        assert!(
            g.success,
            "call_it reverted for id {} len {}",
            id,
            name.len()
        );
        assert_eq!(
            g.output,
            s.output,
            "call_it output differs for id {} len {}",
            id,
            name.len()
        );
    }
}

#[test]
fn fuzz_random_storage_layout_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

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
        let nfields = 3 + (rng.next_u64() % 6) as usize;
        let fields: Vec<(&str, &str, bool)> = (0..nfields)
            .map(|_| types[(rng.next_u64() as usize) % types.len()])
            .collect();

        let mut gum = String::from("contract C:\n");
        for (i, (g, _, _)) in fields.iter().enumerate() {
            gum.push_str(&format!("    {} f{}\n", g, i));
        }
        for (i, (g, _, arith)) in fields.iter().enumerate() {
            gum.push_str(&format!(
                "\n    export fn s{i}({g} v):\n        C.f{i} = v\n",
                i = i,
                g = g
            ));
            gum.push_str(&format!(
                "\n    export fn g{i}() -> {g}:\n        return C.f{i}\n",
                i = i,
                g = g
            ));
            if *arith {
                gum.push_str(&format!(
                    "\n    export fn a{i}({g} v):\n        C.f{i} = C.f{i} + v\n",
                    i = i,
                    g = g
                ));
                gum.push_str(&format!(
                    "\n    export fn b{i}({g} v):\n        C.f{i} = C.f{i} - v\n",
                    i = i,
                    g = g
                ));

                gum.push_str(&format!(
                    "\n    export fn m{i}():\n        C.f{i} = 3 * C.f{i}\n",
                    i = i
                ));
            }
        }

        let mut sol = String::from(
            "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract C {\n",
        );
        for (i, (_, s, _)) in fields.iter().enumerate() {
            sol.push_str(&format!("    {} f{};\n", s, i));
        }
        for (i, (_, s, arith)) in fields.iter().enumerate() {
            sol.push_str(&format!(
                "    function s{i}({s} v) external {{ f{i} = v; }}\n",
                i = i,
                s = s
            ));
            sol.push_str(&format!(
                "    function g{i}() external view returns ({s}) {{ return f{i}; }}\n",
                i = i,
                s = s
            ));
            if *arith {
                sol.push_str(&format!(
                    "    function a{i}({s} v) external {{ f{i} = f{i} + v; }}\n",
                    i = i,
                    s = s
                ));
                sol.push_str(&format!(
                    "    function b{i}({s} v) external {{ f{i} = f{i} - v; }}\n",
                    i = i,
                    s = s
                ));
                sol.push_str(&format!(
                    "    function m{i}() external {{ f{i} = 3 * f{i}; }}\n",
                    i = i
                ));
            }
        }
        sol.push_str("}\n");

        let gbc = gum_creation_bytecode(&gum, &solc, false);
        let sbc = sol_creation_bytecode(&sol, &solc);
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gbc);
        let sa = deploy(&mut sdb, sbc);

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
                    low | !mask
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
                assert_eq!(
                    g.success, s.success,
                    "seed {}: {} success mismatch",
                    seed, sig
                );
                assert_eq!(
                    g.output, s.output,
                    "seed {}: {} value diverged\ngum:\n{}",
                    seed, sig, gum
                );
            }
        };

        for _ in 0..140 {
            let k = (rng.next_u64() as usize) % fields.len();
            let (g, s, arith) = fields[k];

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
                let gg = call(&mut gdb, ga, selector(&format!("g{}()", k)).to_vec());
                let sg = call(&mut sdb, sa, selector(&format!("g{}()", k)).to_vec());
                assert_eq!(
                    gg.output, sg.output,
                    "seed {}: after {} field {} diverged\ngum:\n{}",
                    seed, sig, k, gum
                );
            }
        }

        getters_match(&mut gdb, &mut sdb);
    }
}

#[test]
fn fuzz_literal_position_arithmetic_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

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
        let mut gum = String::from("contract C:\n");
        let mut sol = String::from(
            "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract C {\n",
        );
        for (oi, op) in ops.iter().enumerate() {
            let c = 3;

            gum.push_str(&format!(
                "    export fn ll{oi}({g} v) -> {g}:\n        return {c} {op} v\n",
                oi = oi,
                g = g,
                c = c,
                op = op
            ));
            gum.push_str(&format!(
                "    export fn lr{oi}({g} v) -> {g}:\n        return v {op} {c}\n",
                oi = oi,
                g = g,
                c = c,
                op = op
            ));
            gum.push_str(&format!(
                "    export fn vv{oi}({g} v, {g} w) -> {g}:\n        return v {op} w\n",
                oi = oi,
                g = g,
                op = op
            ));
            sol.push_str(&format!(
                "  function ll{oi}({s} v) external pure returns ({s}) {{ return {c} {op} v; }}\n",
                oi = oi,
                s = s,
                c = c,
                op = op
            ));
            sol.push_str(&format!(
                "  function lr{oi}({s} v) external pure returns ({s}) {{ return v {op} {c}; }}\n",
                oi = oi,
                s = s,
                c = c,
                op = op
            ));
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
                    (
                        sig.clone(),
                        encode_words(&sig, &[encode_arg(&mut rng, g, bits)]),
                    )
                }
                1 => {
                    let sig = format!("lr{}({})", oi, s);
                    (
                        sig.clone(),
                        encode_words(&sig, &[encode_arg(&mut rng, g, bits)]),
                    )
                }
                _ => {
                    let sig = format!("vv{}({},{})", oi, s, s);
                    (
                        sig.clone(),
                        encode_words(
                            &sig,
                            &[encode_arg(&mut rng, g, bits), encode_arg(&mut rng, g, bits)],
                        ),
                    )
                }
            };
            let gr = call(&mut gdb, ga, data.clone());
            let sr = call(&mut sdb, sa, data);
            assert_eq!(
                gr.success, sr.success,
                "{}: success mismatch\ngum:\n{}",
                sig, gum
            );
            if gr.success {
                assert_eq!(
                    gr.output, sr.output,
                    "{}: value diverged\ngum:\n{}",
                    sig, gum
                );
            }
        }
    }
}

#[test]
fn fuzz_dynamic_abi_roundtrip_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    {
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(
            &mut gdb,
            gum_creation_bytecode(include_str!("fixtures/abi/dyn_struct.gum"), &solc, false),
        );
        let sa = deploy(
            &mut sdb,
            sol_creation_bytecode(include_str!("fixtures/abi/dyn_struct.sol"), &solc),
        );

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

    {
        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(
            &mut gdb,
            gum_creation_bytecode_for(
                include_str!("fixtures/abi/nested_array.gum"),
                &solc,
                false,
                "N",
            ),
        );
        let sa = deploy(
            &mut sdb,
            sol_creation_bytecode_for(include_str!("fixtures/abi/nested_array.sol"), &solc, "N"),
        );

        let encode = |rows: &[Vec<U256>]| -> Vec<u8> {
            let mut data = selector("echo(uint256[][])").to_vec();
            data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
            data.extend_from_slice(&U256::from(rows.len()).to_be_bytes::<32>());

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

#[test]
fn fuzz_enum_match_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    for prog in 0..40u64 {
        let mut rng = Rng(0x5a7c_0000 ^ prog);
        let nvar = 3 + (rng.next_u64() % 3) as usize;
        let consts: Vec<u64> = (0..nvar).map(|_| rng.next_u64() % 1000).collect();

        let mut gum = String::from("enum E:\n");
        for i in 0..nvar {
            gum.push_str(&format!("    V{}\n", i));
        }
        gum.push_str("\ncontract C:\n    export fn tag(E s) -> u256:\n        match s:\n");
        for i in 0..nvar {
            gum.push_str(&format!(
                "            V{}:\n                return {}\n",
                i, consts[i]
            ));
        }
        gum.push_str("        return 0\n\n    export fn fold([E] xs) -> u256:\n        mut u256 acc = 0\n        for x in xs:\n            match x:\n");
        for i in 0..nvar {
            gum.push_str(&format!(
                "                V{}:\n                    acc = acc + {}\n",
                i, consts[i]
            ));
        }
        gum.push_str("        return acc\n");

        let mut sol = String::from(
            "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract C {\n    enum E { ",
        );
        sol.push_str(
            &(0..nvar)
                .map(|i| format!("V{}", i))
                .collect::<Vec<_>>()
                .join(", "),
        );
        sol.push_str(" }\n    function tag(E s) external pure returns (uint256) {\n");
        for i in 0..nvar {
            sol.push_str(&format!(
                "        if (s == E.V{}) return {};\n",
                i, consts[i]
            ));
        }
        sol.push_str("        return 0;\n    }\n    function fold(E[] calldata xs) external pure returns (uint256) {\n        uint256 acc = 0;\n        for (uint256 i = 0; i < xs.length; i++) {\n");
        for i in 0..nvar {
            sol.push_str(&format!(
                "            if (xs[i] == E.V{}) acc += {};\n",
                i, consts[i]
            ));
        }
        sol.push_str("        }\n        return acc;\n    }\n}\n");

        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gum_creation_bytecode(&gum, &solc, false));
        let sa = deploy(&mut sdb, sol_creation_bytecode(&sol, &solc));

        for v in 0..(nvar as u64 + 4) {
            let mut w = [0u8; 32];
            w[31] = v as u8;
            let data = encode_words("tag(uint8)", &[w]);
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(
                g.success, s.success,
                "prog {}: tag({}) success\ngum:\n{}",
                prog, v, gum
            );
            assert_eq!(
                g.output, s.output,
                "prog {}: tag({}) value\ngum:\n{}",
                prog, v, gum
            );
        }

        for _ in 0..12 {
            let n = (rng.next_u64() % 8) as usize;
            let span = if rng.next_u64() % 3 == 0 {
                nvar as u64 + 3
            } else {
                nvar as u64
            };
            let elems: Vec<u8> = (0..n).map(|_| (rng.next_u64() % span) as u8).collect();
            let mut data = selector("fold(uint8[])").to_vec();
            data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
            data.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
            for e in &elems {
                let mut w = [0u8; 32];
                w[31] = *e;
                data.extend_from_slice(&w);
            }
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(
                g.success, s.success,
                "prog {}: fold success ({:?})\ngum:\n{}",
                prog, elems, gum
            );
            assert_eq!(
                g.output, s.output,
                "prog {}: fold value ({:?})\ngum:\n{}",
                prog, elems, gum
            );
        }
    }
}

enum AbiArg {
    Bytes(Vec<u8>),
    Word([u8; 32]),
}

fn abi_encode_mixed(sig: &str, args: &[AbiArg]) -> Vec<u8> {
    let head_size = args.len() * 32;
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for a in args {
        match a {
            AbiArg::Word(w) => head.extend_from_slice(w),
            AbiArg::Bytes(b) => {
                let off = head_size + tail.len();
                head.extend_from_slice(&U256::from(off).to_be_bytes::<32>());
                tail.extend_from_slice(&U256::from(b.len()).to_be_bytes::<32>());
                tail.extend_from_slice(b);
                let pad = (32 - (b.len() % 32)) % 32;
                tail.extend(std::iter::repeat(0u8).take(pad));
            }
        }
    }
    let mut out = selector(sig).to_vec();
    out.extend(head);
    out.extend(tail);
    out
}

#[test]
fn fuzz_string_ops_match_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(
        &mut gdb,
        gum_creation_bytecode(include_str!("fixtures/string/fuzz.gum"), &solc, false),
    );
    let sa = deploy(
        &mut sdb,
        sol_creation_bytecode(include_str!("fixtures/string/fuzz.sol"), &solc),
    );

    let mut rng = Rng(0x57ce_0000);
    let rand_str = |rng: &mut Rng| -> Vec<u8> {
        let len = (rng.next_u64() % 96) as usize;
        (0..len).map(|_| (rng.next_u64() & 0xff) as u8).collect()
    };
    let diff =
        |gdb: &mut Db, sdb: &mut Db, gsig: &str, ssig: &str, args: &[AbiArg], label: &str| {
            let gd = abi_encode_mixed(gsig, args);
            let sd = abi_encode_mixed(ssig, args);
            let g = call(gdb, ga, gd);
            let s = call(sdb, sa, sd);
            assert_eq!(g.success, s.success, "{}: success mismatch", label);
            if g.success {
                assert_eq!(g.output, s.output, "{}: output diverged", label);
            }
        };

    for _ in 0..250 {
        let a = rand_str(&mut rng);
        let b = if rng.next_u64() % 4 == 0 {
            a.clone()
        } else {
            rand_str(&mut rng)
        };

        diff(
            &mut gdb,
            &mut sdb,
            "cat(string,string)",
            "cat(string,string)",
            &[AbiArg::Bytes(a.clone()), AbiArg::Bytes(b.clone())],
            "cat",
        );
        diff(
            &mut gdb,
            &mut sdb,
            "same(string,string)",
            "same(string,string)",
            &[AbiArg::Bytes(a.clone()), AbiArg::Bytes(b.clone())],
            "same",
        );
        diff(
            &mut gdb,
            &mut sdb,
            "len(string)",
            "len(string)",
            &[AbiArg::Bytes(a.clone())],
            "len",
        );

        let i = rng.next_u64() % (a.len() as u64 + 3);
        diff(
            &mut gdb,
            &mut sdb,
            "at(string,uint256)",
            "at(string,uint256)",
            &[
                AbiArg::Bytes(a.clone()),
                AbiArg::Word(U256::from(i).to_be_bytes::<32>()),
            ],
            "at",
        );

        let s0 = rng.next_u64() % (a.len() as u64 + 3);
        let e0 = rng.next_u64() % (a.len() as u64 + 3);
        diff(
            &mut gdb,
            &mut sdb,
            "cut(string,uint256,uint256)",
            "cut(bytes,uint256,uint256)",
            &[
                AbiArg::Bytes(a.clone()),
                AbiArg::Word(U256::from(s0).to_be_bytes::<32>()),
                AbiArg::Word(U256::from(e0).to_be_bytes::<32>()),
            ],
            "cut",
        );

        let n = rng.next_u256(true);
        let out = call(
            &mut gdb,
            ga,
            abi_encode_mixed("numstr(uint256)", &[AbiArg::Word(n.to_be_bytes::<32>())]),
        )
        .output;
        let len = U256::from_be_slice(&out[32..64]).to::<usize>();
        let got = String::from_utf8(out[64..64 + len].to_vec()).unwrap();
        assert_eq!(got, n.to_string(), "numstr({}) wrong", n);
    }
}

#[test]
fn enum_field_in_struct_matches_solidity_and_is_bounds_checked() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum = include_str!("fixtures/enum/struct.gum");
    let sol = include_str!("fixtures/enum/struct.sol");
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum_creation_bytecode(gum, &solc, false));
    let sa = deploy(&mut sdb, sol_creation_bytecode(sol, &solc));

    let encode = |sig: &str, x: u64, tag: u8| -> Vec<u8> {
        let mut d = selector(sig).to_vec();
        d.extend_from_slice(&U256::from(x).to_be_bytes::<32>());
        let mut tw = [0u8; 32];
        tw[31] = tag;
        d.extend_from_slice(&tw);
        d
    };

    for (x, tag) in [(0u64, 0u8), (7, 1), (99, 2)] {
        for sig in ["get_x((uint256,uint8))", "get_tag((uint256,uint8))"] {
            let g = call(&mut gdb, ga, encode(sig, x, tag));
            let s = call(&mut sdb, sa, encode(sig, x, tag));
            assert!(
                g.success && s.success,
                "{} should succeed (x={} tag={})",
                sig,
                x,
                tag
            );
            assert_eq!(g.output, s.output, "{} output (x={} tag={})", sig, x, tag);
        }
    }

    for tag in [3u8, 200, 255] {
        let g = call(&mut gdb, ga, encode("get_tag((uint256,uint8))", 5, tag));
        let s = call(&mut sdb, sa, encode("get_tag((uint256,uint8))", 5, tag));
        assert_eq!(
            g.success, s.success,
            "get_tag out-of-range must agree (tag={})",
            tag
        );
        assert!(!g.success, "get_tag(tag={}) must revert", tag);

        let g = call(&mut gdb, ga, encode("get_x((uint256,uint8))", 5, tag));
        assert!(
            !g.success,
            "gum eagerly rejects an out-of-range enum field (tag={})",
            tag
        );
    }
}

#[test]
fn enum_constructor_arg_is_bounds_checked() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = "enum E:\n    A\n    B\n\ncontract C:\n    const E kind\n    u256 v\n\n    fn new(E k):\n        C.kind = k\n        C.v = 1\n\n    export fn get() -> u256:\n        return C.v\n";
    let base = gum_creation_bytecode(src, &solc, false);

    for (tag, ok) in [(0u64, true), (1, true), (2, false), (200, false)] {
        let mut code = base.clone();
        code.extend_from_slice(&U256::from(tag).to_be_bytes::<32>());
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let r = try_deploy(&mut db, code);
        assert_eq!(
            r.is_some(),
            ok,
            "constructor tag={} expected deploy_ok={}",
            tag,
            ok
        );
    }
}

#[test]
fn std_math_utilities_compute() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/misc/math_utils.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let two = |sig: &str, a: u64, b: u64| -> Vec<u8> {
        let mut d = selector(sig).to_vec();
        d.extend_from_slice(&U256::from(a).to_be_bytes::<32>());
        d.extend_from_slice(&U256::from(b).to_be_bytes::<32>());
        d
    };
    let val = |db: &mut Db, data: Vec<u8>| U256::from_be_slice(&call(db, c, data).output);
    assert_eq!(
        val(&mut db, two("f_pow(uint256,uint256)", 2, 10)),
        U256::from(1024u64)
    );
    assert_eq!(
        val(&mut db, two("f_gcd(uint256,uint256)", 48, 18)),
        U256::from(6u64)
    );
    assert_eq!(
        val(&mut db, two("f_avg(uint256,uint256)", 10, 21)),
        U256::from(15u64)
    );
    let mut d = selector("f_sqrt(uint256)").to_vec();
    d.extend_from_slice(&U256::from(144u64).to_be_bytes::<32>());
    assert_eq!(
        U256::from_be_slice(&call(&mut db, c, d).output),
        U256::from(12u64)
    );
}

#[test]
fn user_class_static_methods_construct() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/misc/static_method.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let r = call(&mut db, c, selector("go()").to_vec());
    assert!(r.success, "Point.build reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(12u64),
        "build(7,5) -> 12"
    );
    let r = call(&mut db, c, selector("zero()").to_vec());
    assert!(r.success);
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(1u64),
        "origin() -> 0+0+1"
    );
}

#[test]
fn user_class_static_value_methods() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/misc/static_value.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let val = |db: &mut Db, sig: &str| -> U256 {
        let r = call(db, c, selector(sig).to_vec());
        assert!(r.success, "{} reverted", sig);
        U256::from_be_slice(&r.output)
    };
    assert_eq!(val(&mut db, "run()"), U256::from(25u64), "combine(10,5)");
    assert_eq!(val(&mut db, "nested()"), U256::from(12u64), "double(double(3))");
    assert_eq!(
        val(&mut db, "on_instance()"),
        U256::from(208u64),
        "double(4) + scaled(2) with seed 100"
    );
}

#[test]
fn construction_forms_all_build_correctly() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/misc/construction_forms.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let val = |db: &mut Db, sig: &str| -> U256 {
        let r = call(db, c, selector(sig).to_vec());
        assert!(r.success, "{} reverted", sig);
        U256::from_be_slice(&r.output)
    };
    assert_eq!(val(&mut db, "from_new()"), U256::from(7u64), "Point.new(3,4).sum");
    assert_eq!(val(&mut db, "from_origin()"), U256::from(7u64), "Point.origin()");
    assert_eq!(val(&mut db, "from_diag()"), U256::from(10u64), "Point.diag(5)");
    assert_eq!(val(&mut db, "from_factory()"), U256::from(2u64), "Point.unit() factory");
    assert_eq!(val(&mut db, "generic_ctor()"), U256::from(42u64), "Box(u256).new(42)");
    assert_eq!(val(&mut db, "generic_assoc()"), U256::from(7u64), "Box(u256).tag()");
}

#[test]
fn std_utilities_work() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/misc/std_utils.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    let r = call(&mut db, c, selector("vec_ops()").to_vec());
    assert!(r.success, "vec_ops reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(119u64));

    let r = call(&mut db, c, selector("is_empty_check()").to_vec());
    assert!(r.success);
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(1u64),
        "fresh Vec is empty"
    );

    let mut d = selector("math(uint256,uint256)").to_vec();
    d.extend_from_slice(&U256::from(8u64).to_be_bytes::<32>());
    d.extend_from_slice(&U256::from(3u64).to_be_bytes::<32>());
    let r = call(&mut db, c, d);
    assert!(r.success, "math (free fns) reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(19u64));

    let r = call(&mut db, c, selector("umax()").to_vec());
    assert_eq!(U256::from_be_slice(&r.output), U256::MAX, "u256.max()");
    let r = call(&mut db, c, selector("umin()").to_vec());
    assert_eq!(U256::from_be_slice(&r.output), U256::ZERO, "u256.min()");
    let r = call(&mut db, c, selector("small_max()").to_vec());
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(255u64),
        "u8.max()"
    );
}

#[test]
fn reentrancy_guard_holds_across_try_catch() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/try/reentrancy.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let g = deploy(&mut db, gum_creation_bytecode_for(src, &solc, false, "G"));
    let benign = deploy(
        &mut db,
        gum_creation_bytecode_for(src, &solc, false, "BenignSink"),
    );
    let resink = deploy(
        &mut db,
        gum_creation_bytecode_for(src, &solc, false, "ReSink"),
    );

    let run =
        |db: &mut Db, sig: &str, sink: Address| call(db, g, encode_words(sig, &[word_addr(sink)]));
    let total = |db: &mut Db| -> U256 {
        U256::from_be_slice(&call(db, g, selector("get_total()").to_vec()).output)
    };

    let r = run(&mut db, "run(address)", benign);
    assert!(r.success, "benign run should succeed");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(7u64));
    assert_eq!(total(&mut db), U256::from(1u64));

    let r = run(&mut db, "run(address)", resink);
    assert!(
        !r.success,
        "reentrant run must revert (guard blocks reentry)"
    );
    assert_eq!(
        total(&mut db),
        U256::from(1u64),
        "reverted run must not change total"
    );

    let r = run(&mut db, "run_try(address)", resink);
    assert!(r.success, "run_try should catch the blocked reentry");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(42u64),
        "catch value"
    );
    assert_eq!(
        total(&mut db),
        U256::from(2u64),
        "exactly one increment, no reentrant double-count"
    );

    let r = run(&mut db, "run_try(address)", benign);
    assert!(r.success, "lock must not be stuck after a caught reentry");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(7u64));
    assert_eq!(total(&mut db), U256::from(3u64));
}

#[derive(Clone)]
enum LBody {
    AddElem,
    Bump(u64),
    Scale(u64),
    CondAdd(u64),
    InnerFor(Vec<LBody>),
}

#[derive(Clone)]
enum LUnit {
    Bump(u64),
    Scale(u64),
    ForEach(Vec<LBody>),
    While(Vec<LBody>),
}

fn gen_lbody(rng: &mut Rng, depth: u32) -> Vec<LBody> {
    let n = 1 + (rng.next_u64() % 3) as usize;
    (0..n)
        .map(|_| match rng.next_u64() % 5 {
            0 => LBody::AddElem,
            1 => LBody::Bump(1 + rng.next_u64() % 5),
            2 => LBody::Scale(1 + rng.next_u64() % 3),
            3 if depth > 0 => LBody::InnerFor(gen_lbody(rng, depth - 1)),
            _ => LBody::CondAdd(rng.next_u64() % 50),
        })
        .collect()
}

fn gen_loop_program(rng: &mut Rng) -> Vec<LUnit> {
    let n = 1 + (rng.next_u64() % 4) as usize;
    (0..n)
        .map(|_| match rng.next_u64() % 4 {
            0 => LUnit::Bump(1 + rng.next_u64() % 5),
            1 => LUnit::Scale(1 + rng.next_u64() % 3),
            2 => LUnit::ForEach(gen_lbody(rng, 1)),
            _ => LUnit::While(gen_lbody(rng, 1)),
        })
        .collect()
}

fn render_lbody(
    body: &[LBody],
    elem: &str,
    idx: &mut usize,
    level: usize,
    gum: bool,
    out: &mut String,
) {
    let ind = "    ".repeat(level);
    for b in body {
        match b {
            LBody::AddElem => out.push_str(&format!(
                "{ind}acc = acc + {elem}{}\n",
                if gum { "" } else { ";" }
            )),
            LBody::Bump(c) => out.push_str(&format!(
                "{ind}acc = acc + {c}{}\n",
                if gum { "" } else { ";" }
            )),
            LBody::Scale(c) => out.push_str(&format!(
                "{ind}acc = acc * {c}{}\n",
                if gum { "" } else { ";" }
            )),
            LBody::CondAdd(k) => {
                if gum {
                    out.push_str(&format!(
                        "{ind}if acc > {k}:\n{ind}    acc = acc + {elem}\n"
                    ));
                } else {
                    out.push_str(&format!("{ind}if (acc > {k}) {{ acc = acc + {elem}; }}\n"));
                }
            }
            LBody::InnerFor(inner) => {
                let j = *idx;
                *idx += 1;
                if gum {
                    out.push_str(&format!("{ind}for e{j} in C.xs:\n"));
                    render_lbody(inner, &format!("e{j}"), idx, level + 1, true, out);
                } else {
                    out.push_str(&format!(
                        "{ind}for (uint256 e{j} = 0; e{j} < xs.length; e{j}++) {{\n"
                    ));
                    render_lbody(inner, &format!("xs[e{j}]"), idx, level + 1, false, out);
                    out.push_str(&format!("{ind}}}\n"));
                }
            }
        }
    }
}

fn render_units(units: &[LUnit], idx: &mut usize, level: usize, gum: bool, out: &mut String) {
    let ind = "    ".repeat(level);
    for u in units {
        match u {
            LUnit::Bump(c) => out.push_str(&format!(
                "{ind}acc = acc + {c}{}\n",
                if gum { "" } else { ";" }
            )),
            LUnit::Scale(c) => out.push_str(&format!(
                "{ind}acc = acc * {c}{}\n",
                if gum { "" } else { ";" }
            )),
            LUnit::ForEach(body) => {
                let j = *idx;
                *idx += 1;
                if gum {
                    out.push_str(&format!("{ind}for e{j} in C.xs:\n"));
                    render_lbody(body, &format!("e{j}"), idx, level + 1, true, out);
                } else {
                    out.push_str(&format!(
                        "{ind}for (uint256 e{j} = 0; e{j} < xs.length; e{j}++) {{\n"
                    ));
                    render_lbody(body, &format!("xs[e{j}]"), idx, level + 1, false, out);
                    out.push_str(&format!("{ind}}}\n"));
                }
            }
            LUnit::While(body) => {
                let j = *idx;
                *idx += 1;
                if gum {
                    out.push_str(&format!(
                        "{ind}mut u256 i{j} = 0\n{ind}while i{j} < C.xs.length:\n"
                    ));
                    render_lbody(body, &format!("C.xs[i{j}]"), idx, level + 1, true, out);
                    out.push_str(&format!("{ind}    i{j} = i{j} + 1\n"));
                } else {
                    out.push_str(&format!(
                        "{ind}uint256 i{j} = 0;\n{ind}while (i{j} < xs.length) {{\n"
                    ));
                    render_lbody(body, &format!("xs[i{j}]"), idx, level + 1, false, out);
                    out.push_str(&format!("{ind}    i{j} = i{j} + 1;\n{ind}}}\n"));
                }
            }
        }
    }
}

fn eval_loop_body(body: &[LBody], elem: U256, xs: &[U256], acc: &mut U256) -> Option<()> {
    for b in body {
        match b {
            LBody::AddElem => *acc = acc.checked_add(elem)?,
            LBody::Bump(c) => *acc = acc.checked_add(U256::from(*c))?,
            LBody::Scale(c) => *acc = acc.checked_mul(U256::from(*c))?,
            LBody::CondAdd(k) => {
                if *acc > U256::from(*k) {
                    *acc = acc.checked_add(elem)?;
                }
            }
            LBody::InnerFor(inner) => {
                for &e in xs {
                    eval_loop_body(inner, e, xs, acc)?;
                }
            }
        }
    }
    Some(())
}

fn eval_loop_program(units: &[LUnit], xs: &[U256], acc: &mut U256) -> Option<()> {
    for u in units {
        match u {
            LUnit::Bump(c) => *acc = acc.checked_add(U256::from(*c))?,
            LUnit::Scale(c) => *acc = acc.checked_mul(U256::from(*c))?,
            LUnit::ForEach(body) | LUnit::While(body) => {
                for &e in xs {
                    eval_loop_body(body, e, xs, acc)?;
                }
            }
        }
    }
    Some(())
}

const CTRL_VARS: usize = 3;

#[derive(Clone)]
enum CtrlStmt {
    Add(usize, u64),
    Assert(u64),
    Ret,
    Try(Vec<CtrlStmt>, Vec<CtrlStmt>),
}

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

fn render_ctrl(stmts: &[CtrlStmt], level: usize, out: &mut String) {
    let ind = "    ".repeat(level);
    let sum = (0..CTRL_VARS)
        .map(|i| format!("r{}", i))
        .collect::<Vec<_>>()
        .join(" + ");
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

enum CtrlFlow {
    Fell,
    Returned(U256),
    Reverted,
}

fn eval_ctrl(stmts: &[CtrlStmt], a: U256, vars: &mut [U256; CTRL_VARS]) -> CtrlFlow {
    for s in stmts {
        match s {
            CtrlStmt::Add(vi, c) => vars[*vi] += U256::from(*c),
            CtrlStmt::Assert(k) => {
                if a >= U256::from(*k) {
                    return CtrlFlow::Reverted;
                }
            }
            CtrlStmt::Ret => {
                return CtrlFlow::Returned(vars.iter().copied().fold(U256::ZERO, |x, y| x + y));
            }
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

        let decls: String = (0..CTRL_VARS)
            .map(|i| format!("        mut u256 r{} = a\n", i))
            .collect();
        let sum = (0..CTRL_VARS)
            .map(|i| format!("r{}", i))
            .collect::<Vec<_>>()
            .join(" + ");

        let via_helper = rng.next_u64() % 2 == 0;
        let mut src = String::from("contract C:\n");
        if via_helper {
            src.push_str(&format!("    fn g(u256 a) -> u256:\n{}", decls));
            render_ctrl(&body, 2, &mut src);
            src.push_str(&format!(
                "        return {sum}\n\n    export fn f(u256 a) -> u256:\n        return C.g(a)\n"
            ));
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
                    assert!(
                        !r.success,
                        "prog {}: a={} expected revert got success\nsrc:\n{}",
                        prog, a, src
                    );
                    continue;
                }
            };
            assert!(
                r.success,
                "prog {}: a={} expected {} got revert\nsrc:\n{}",
                prog, a, expected, src
            );
            assert_eq!(
                U256::from_be_slice(&r.output),
                expected,
                "prog {}: a={} value diverged\nsrc:\n{}",
                prog,
                a,
                src
            );
        }
    }
}

#[test]
fn fuzz_storage_loops_match_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping fuzz: no solc");
            return;
        }
    };

    for prog in 0..40u64 {
        let mut rng = Rng(0x100b_0000 ^ prog);
        let units = gen_loop_program(&mut rng);

        let mut gum = String::from(
            "contract C:\n    [u256] xs\n\n    export fn push(u256 v):\n        C.xs.push(v)\n\n    export fn run(u256 seed) -> u256:\n        mut u256 acc = seed\n",
        );
        let mut gidx = 0usize;
        render_units(&units, &mut gidx, 2, true, &mut gum);
        gum.push_str("        return acc\n");

        let mut sol = String::from(
            "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract C {\n    uint256[] xs;\n    function push(uint256 v) external { xs.push(v); }\n    function run(uint256 seed) external view returns (uint256) {\n        uint256 acc = seed;\n",
        );
        let mut sidx = 0usize;
        render_units(&units, &mut sidx, 2, false, &mut sol);
        sol.push_str("        return acc;\n    }\n}\n");

        let mut gdb: Db = CacheDB::new(EmptyDB::default());
        let mut sdb: Db = CacheDB::new(EmptyDB::default());
        let ga = deploy(&mut gdb, gum_creation_bytecode(&gum, &solc, false));
        let sa = deploy(&mut sdb, sol_creation_bytecode(&sol, &solc));

        let n = (rng.next_u64() % 8) as usize;
        let mut xs: Vec<U256> = Vec::new();
        for _ in 0..n {
            let v = rng.next_u256(true);
            xs.push(v);
            let data = encode_words("push(uint256)", &[v.to_be_bytes::<32>()]);
            assert!(call(&mut gdb, ga, data.clone()).success);
            assert!(call(&mut sdb, sa, data).success);
        }

        for _ in 0..12 {
            let seed = rng.next_u256(true);
            let data = encode_words("run(uint256)", &[seed.to_be_bytes::<32>()]);
            let g = call(&mut gdb, ga, data.clone());
            let s = call(&mut sdb, sa, data);
            assert_eq!(
                g.success, s.success,
                "prog {}: run success mismatch\ngum:\n{}",
                prog, gum
            );
            if g.success {
                assert_eq!(
                    g.output, s.output,
                    "prog {}: run value diverged\ngum:\n{}",
                    prog, gum
                );

                let mut acc = seed;
                if let Some(()) = eval_loop_program(&units, &xs, &mut acc) {
                    assert_eq!(
                        U256::from_be_slice(&g.output),
                        acc,
                        "prog {}: oracle disagrees\ngum:\n{}",
                        prog,
                        gum
                    );
                }
            }
        }
    }
}

#[test]
fn try_that_returns_and_writes_back_is_caught_through_an_internal_call() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/try/internal_return.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let call_f = |db: &mut Db, a: u64| -> (bool, U256) {
        let mut d = selector("f(uint256)").to_vec();
        d.extend_from_slice(&U256::from(a).to_be_bytes::<32>());
        let r = call(db, c, d);
        (r.success, U256::from_be_slice(&r.output))
    };

    assert_eq!(call_f(&mut db, 0), (true, U256::from(4u64)));
    assert_eq!(call_f(&mut db, 1), (true, U256::from(5u64)));

    assert_eq!(call_f(&mut db, 5), (true, U256::from(8u64)));
    assert_eq!(call_f(&mut db, 20), (true, U256::from(23u64)));
}

#[test]
fn try_writes_back_multiple_mixed_type_variables_and_returns_another_type() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/try/multi_writeback.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let call_f = |db: &mut Db, a: u64| -> (bool, String) {
        let mut d = selector("f(uint256)").to_vec();
        d.extend_from_slice(&U256::from(a).to_be_bytes::<32>());
        let r = call(db, c, d);

        let len = U256::from_be_slice(&r.output[32..64]).to::<usize>();
        (
            r.success,
            String::from_utf8(r.output[64..64 + len].to_vec()).unwrap(),
        )
    };

    assert_eq!(call_f(&mut db, 0), (true, "returned".to_string()));

    assert_eq!(call_f(&mut db, 5), (true, "105".to_string()));
    assert_eq!(call_f(&mut db, 42), (true, "142".to_string()));
}

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
    let gum = include_str!("fixtures/storage/signed_containers.gum");
    let sol = include_str!("fixtures/storage/signed_containers.sol");
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
        (
            "mapping",
            "setm(uint256,int32)",
            vec![k, neg(-2_000_000_000)],
            "addm(uint256,int32)",
            vec![k, neg(-500_000_000)],
        ),
        (
            "array",
            "pusha(int32)",
            vec![neg(-2_000_000_000)],
            "adda(uint256,int32)",
            vec![word_u256(U256::ZERO), neg(-500_000_000)],
        ),
        (
            "struct",
            "setp(int32)",
            vec![neg(-2_000_000_000)],
            "addp(int32)",
            vec![neg(-500_000_000)],
        ),
    ] {
        let (gs, ss) = step(&mut gdb, &mut sdb, setsig, &setargs);
        assert!(gs && ss, "{}: set should succeed", label);
        let (ga_ok, sa_ok) = step(&mut gdb, &mut sdb, addsig, &addargs);
        assert_eq!(
            ga_ok, sa_ok,
            "{}: underflow revert must agree with Solidity",
            label
        );
        assert!(!ga_ok, "{}: underflow must revert", label);
    }
}

#[test]
fn const_fields_are_baked_in_per_deployment() {
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
        assert_eq!(
            Address::from_slice(&r.output[12..32]),
            owner,
            "wrong const owner"
        );

        let r = call(&mut db, addr, encode("get_cap()", &[]));
        assert!(r.success, "get_cap() reverted");
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(cap),
            "wrong const cap"
        );

        let r = call(&mut db, addr, encode("bump()", &[]));
        assert!(r.success, "bump() reverted");
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(1u64),
            "counter should be 1"
        );
        let r = call(&mut db, addr, encode("bump()", &[]));
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(2u64),
            "counter should be 2"
        );

        let r = call(&mut db, addr, encode("get_cap()", &[]));
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(cap),
            "const field changed after a write"
        );
    }
}

#[test]
fn super_calls_the_overridden_method() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let src = "class Base:\n    u256 v\n\n    fn label(self) -> u256:\n        return 10\n\n\
               [Base]\nclass Child:\n    fn label(self) -> u256:\n        return super.label() + 5\n\n\
               contract C:\n    u256 out\n\n    export fn run() -> u256:\n        \
               mut Child c = Child.new()\n        return c.label()\n";
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let addr = deploy(&mut db, gum_creation_bytecode(src, &solc, false));
    let r = call(&mut db, addr, encode("run()", &[]));
    assert!(r.success, "run() reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(15u64),
        "super.label() should give 10 + 5"
    );
}

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
    assert!(
        call(&mut db, addr, encode("fill()", &[])).success,
        "fill reverted"
    );

    let n = call(&mut db, addr, encode("copy_len()", &[]));
    assert_eq!(
        U256::from_be_slice(&n.output),
        U256::from(3u64),
        "copied length"
    );

    let s = call(&mut db, addr, encode("copy_sum()", &[]));
    assert_eq!(
        U256::from_be_slice(&s.output),
        U256::from(24u64),
        "7 + 8 + 9"
    );

    for (i, want) in [(0u64, 7u64), (1, 8), (2, 9)] {
        let r = call(&mut db, addr, encode("copy_at(uint256)", &[U256::from(i)]));
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(want),
            "copied element {}",
            i
        );
    }

    let p = call(&mut db, addr, encode("copy_small_sum()", &[]));
    assert_eq!(
        U256::from_be_slice(&p.output),
        U256::from(10u64),
        "1 + 2 + 3 + 4 of a packed u8 array"
    );

    let oob = call(
        &mut db,
        addr,
        encode("copy_at(uint256)", &[U256::from(3u64)]),
    );
    assert!(!oob.success, "index 3 of a 3 element copy must revert");
}

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

    let who: Address = "0x00000000000000000000000000000000cafebabe"
        .parse()
        .unwrap();
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
        assert_eq!(
            gr.output, sr.output,
            "{} return data differs from solidity",
            f
        );
    }

    let fa = call(&mut gdb, g, encode_words(&format!("fa({})", tuple), &arg));
    assert_eq!(
        U256::from_be_slice(&fa.output),
        U256::from(11u64),
        "field a"
    );
    let fd = call(&mut gdb, g, encode_words(&format!("fd({})", tuple), &arg));
    assert_eq!(
        U256::from_be_slice(&fd.output),
        U256::from_be_slice(who.as_slice()),
        "field d"
    );

    let short = encode_words(&format!("fa({})", tuple), &arg[..4]);
    assert!(
        !call(&mut gdb, g, short).success,
        "a truncated tuple must revert"
    );

    let mut mixed = vec![word_u256(U256::from(5u64))];
    mixed.extend_from_slice(&arg);
    mixed.push(word_u256(U256::from(7u64)));
    let msig = format!("mix(uint256,{},uint256)", tuple);
    let gm = call(&mut gdb, g, encode_words(&msig, &mixed));
    let sm = call(&mut sdb, s, encode_words(&msig, &mixed));
    assert!(gm.success && sm.success, "mix reverted");
    assert_eq!(
        gm.output, sm.output,
        "mix return data differs from solidity"
    );
    assert_eq!(
        U256::from_be_slice(&gm.output),
        U256::from(34u64),
        "5 + 22 + 7"
    );
}

#[test]
fn a_struct_constructor_arg_matches_solidity() {
    let solc = match solc_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let who: Address = "0x000000000000000000000000000000000badf00d"
        .parse()
        .unwrap();
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

    for (f, want) in [
        ("get_a()", U256::from(41u64)),
        ("get_b()", U256::from(42u64)),
    ] {
        let gr = call(&mut gdb, g, encode(f, &[]));
        let sr = call(&mut sdb, s, encode(f, &[]));
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
        assert_eq!(U256::from_be_slice(&gr.output), want, "{}", f);
    }
    let gd = call(&mut gdb, g, encode("get_d()", &[]));
    let sd = call(&mut sdb, s, encode("get_d()", &[]));
    assert_eq!(gd.output, sd.output, "get_d differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gd.output),
        U256::from_be_slice(who.as_slice()),
        "address field"
    );
}

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
    let s = deploy(
        &mut sdb,
        sol_creation_bytecode_for(SOL_STRUCT_DEPLOY, &solc, "Parent"),
    );

    let arg = [word_u256(U256::from(3u64)), word_u256(U256::from(77u64))];
    let sig = "make_and_read((uint128,uint256))";
    let gr = call(&mut gdb, g, encode_words(sig, &arg));
    let sr = call(&mut sdb, s, encode_words(sig, &arg));
    assert!(sr.success, "solidity make_and_read reverted");
    assert!(gr.success, "gum make_and_read reverted");
    assert_eq!(gr.output, sr.output, "make_and_read differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(77u64),
        "child read field b"
    );
}

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
    let sink = deploy(
        &mut db,
        sol_creation_bytecode_for(SOL_IFACE_SINK, &solc, "Sink"),
    );
    let caller = deploy_named(&mut db, &solc, GUM_IFACE_CALL, "Caller");

    let r = call(
        &mut db,
        caller,
        encode_words(
            "fwd(address,(uint128,uint256))",
            &[
                word_addr(sink),
                word_u256(U256::from(4u64)),
                word_u256(U256::from(38u64)),
            ],
        ),
    );
    assert!(r.success, "fwd reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(42u64),
        "solidity read the tuple as 4 + 38"
    );

    let mut sd = selector("fwd_str(address,string)").to_vec();
    sd.extend_from_slice(&word_addr(sink));
    sd.extend_from_slice(&word_u256(U256::from(64u64)));
    sd.extend_from_slice(&word_u256(U256::from(5u64)));
    let mut w = [0u8; 32];
    w[..5].copy_from_slice(b"hello");
    sd.extend_from_slice(&w);
    let r = call(&mut db, caller, sd);
    assert!(r.success, "fwd_str reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(5u64),
        "solidity read the string length"
    );

    let mut ad = selector("fwd_arr(address,uint256[])").to_vec();
    ad.extend_from_slice(&word_addr(sink));
    ad.extend_from_slice(&word_u256(U256::from(64u64)));
    ad.extend_from_slice(&word_u256(U256::from(3u64)));
    for v in [10u64, 20, 30] {
        ad.extend_from_slice(&word_u256(U256::from(v)));
    }
    let r = call(&mut db, caller, ad);
    assert!(r.success, "fwd_arr reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(60u64),
        "solidity summed the array"
    );

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
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(33u64),
        "solidity summed 1+10+2+20 over the tuple array"
    );

    let r = call(
        &mut db,
        caller,
        encode_words(
            "fwd_mk(address,uint256)",
            &[word_addr(sink), word_u256(U256::from(99u64))],
        ),
    );
    assert!(r.success, "fwd_mk reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(99u64),
        "gum decoded field b of solidity's returned tuple"
    );

    let r = call(
        &mut db,
        caller,
        encode_words("fwd_name_len(address)", &[word_addr(sink)]),
    );
    assert!(r.success, "fwd_name_len reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(7u64),
        "length of \"gumball\", not the offset word"
    );

    let r = call(
        &mut db,
        caller,
        encode_words("fwd_name(address)", &[word_addr(sink)]),
    );
    assert!(r.success, "fwd_name reverted");

    assert_eq!(
        &r.output[64..71],
        b"gumball",
        "the string itself survived the round trip"
    );
    assert_eq!(
        U256::from_be_slice(&r.output[32..64]),
        U256::from(7u64),
        "returned length word"
    );

    let r = call(
        &mut db,
        caller,
        encode_words("fwd_nums_sum(address)", &[word_addr(sink)]),
    );
    assert!(r.success, "fwd_nums_sum reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(18u64),
        "5 + 6 + 7 from a returned uint256[]"
    );

    for (i, want) in [(0u64, 111u64), (1, 222)] {
        let r = call(
            &mut db,
            caller,
            encode_words(
                "fwd_pairs_b(address,uint256)",
                &[word_addr(sink), word_u256(U256::from(i))],
            ),
        );
        assert!(r.success, "fwd_pairs_b reverted");
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(want),
            "field b of returned tuple array element {}",
            i
        );
    }
}

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
        "0x00000000000000000000000000000000cafebabe"
            .parse()
            .unwrap(),
        "0x000000000000000000000000000000000badf00d"
            .parse()
            .unwrap(),
    ];

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

    for (f, i, want) in [
        ("at_a", 0u64, U256::from(1u64)),
        ("at_a", 1, U256::from(2u64)),
    ] {
        let sig = format!("{}({},uint256)", f, tup);
        let mut d = selector(&sig).to_vec();
        d.extend_from_slice(&word_u256(U256::from(64u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "{}[{}] differs from solidity", f, i);
        assert_eq!(
            U256::from_be_slice(&gr.output),
            want,
            "{}[{}] by value",
            f,
            i
        );
    }

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
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(200u64),
            "element 1's b after copying it over element 0"
        );
    }

    let mut d = selector(&format!("at_c({},uint256)", tup)).to_vec();
    d.extend_from_slice(&word_u256(U256::from(64u64)));
    d.extend_from_slice(&word_u256(U256::from(1u64)));
    d.extend_from_slice(&arr);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "at_c differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from_be_slice(who[1].as_slice()),
        "address field of element 1"
    );

    let mut d = selector(&format!("bump({},uint256,uint128)", tup)).to_vec();
    d.extend_from_slice(&word_u256(U256::from(96u64)));
    d.extend_from_slice(&word_u256(U256::from(1u64)));
    d.extend_from_slice(&word_u256(U256::from(9u64)));
    d.extend_from_slice(&arr);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum bump reverted");
    assert_eq!(gr.output, sr.output, "bump differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(9u64),
        "wrote and read back element 1 field a"
    );

    let mut d = selector(&format!("at_a({},uint256)", tup)).to_vec();
    d.extend_from_slice(&word_u256(U256::from(64u64)));
    d.extend_from_slice(&word_u256(U256::from(2u64)));
    d.extend_from_slice(&arr);
    assert!(
        !call(&mut gdb, g, d).success,
        "index 2 of a 2 element array must revert"
    );
}

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

    let caller: Address = "0x00000000000000000000000000000000deadbeef"
        .parse()
        .unwrap();
    let gr = call_from(&mut gdb, caller, g, encode("who()", &[]));
    let sr = call_from(&mut sdb, caller, s, encode("who()", &[]));
    assert_eq!(gr.output, sr.output, "who() differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from_be_slice(caller.as_slice()),
        "sender is the caller"
    );

    let gr = call_with_value(
        &mut gdb,
        caller,
        g,
        encode("amount()", &[]),
        U256::from(1234u64),
    );
    assert!(gr.success, "amount() reverted");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(1234u64),
        "value is the attached wei"
    );

    let gr = call(&mut gdb, g, encode("me()", &[]));
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from_be_slice(g.as_slice()),
        "gum me() is its own address"
    );
    let sr = call(&mut sdb, s, encode("me()", &[]));
    assert_eq!(
        U256::from_be_slice(&sr.output),
        U256::from_be_slice(s.as_slice()),
        "sol me() is its own address"
    );

    for f in ["when()", "height()"] {
        let gr = call(&mut gdb, g, encode(f, &[]));
        let sr = call(&mut sdb, s, encode(f, &[]));
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
    }
}

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

    for tag in [0u64, 1, 2] {
        let d = encode(
            "after_enum(uint8,uint256)",
            &[U256::from(tag), U256::from(42u64)],
        );
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(gr.success, "after_enum reverted");
        assert_eq!(
            gr.output, sr.output,
            "after_enum({}) differs from solidity",
            tag
        );
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(42u64),
            "the argument after an enum survives"
        );
    }

    let d = encode(
        "between(uint256,uint8,uint256)",
        &[U256::from(7u64), U256::from(1u64), U256::from(9u64)],
    );
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "between differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(16u64),
        "7 + 9 across an enum"
    );

    for (tag, want) in [(0u64, 10u64), (1, 20), (2, 30)] {
        let d = encode("tag(uint8)", &[U256::from(tag)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "tag({}) differs from solidity", tag);
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(want),
            "match on tag {}",
            tag
        );
    }

    for tag in [0u64, 1, 2] {
        let d = encode("echo(uint8)", &[U256::from(tag)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "echo({}) differs from solidity", tag);
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(tag),
            "echo returns the tag"
        );
    }

    for (x, want) in [(0u64, 0u64), (5, 2)] {
        let d = encode("pick(uint256)", &[U256::from(x)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "pick({}) differs from solidity", x);
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(want),
            "pick returns a tag"
        );
    }

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
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(3u64),
        "three Closed in the array"
    );
}

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
    let prober = deploy(
        &mut db,
        sol_creation_bytecode_for(SOL_PROBER, &solc, "Prober"),
    );

    let probe = |db: &mut Db, inner: Vec<u8>| -> bool {
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

    assert!(
        probe(&mut db, encode("get_total()", &[])),
        "get_total must survive STATICCALL"
    );
    assert!(
        probe(
            &mut db,
            encode_words("balance_of(address)", &[word_addr(deployer())])
        ),
        "balance_of must survive STATICCALL"
    );
    assert!(
        probe(&mut db, encode("double(uint256)", &[U256::from(21u64)])),
        "double must survive STATICCALL"
    );
    assert!(
        probe(&mut db, encode("sender()", &[])),
        "sender must survive STATICCALL"
    );

    assert!(
        !probe(&mut db, encode("set_total(uint256)", &[U256::from(5u64)])),
        "set_total writes storage, so STATICCALL must reject it"
    );

    assert!(
        call(
            &mut db,
            v,
            encode("set_total(uint256)", &[U256::from(77u64)])
        )
        .success
    );
    let r = call(&mut db, v, encode("get_total()", &[]));
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(77u64),
        "get_total still reads storage"
    );
    let r = call(&mut db, v, encode("double(uint256)", &[U256::from(21u64)]));
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(42u64),
        "double still computes"
    );
}

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

    for tag in [0u64, 1, 2] {
        let d = encode("set_state(uint8)", &[U256::from(tag)]);
        assert!(
            call(&mut gdb, g, d.clone()).success,
            "gum set_state reverted"
        );
        assert!(call(&mut sdb, s, d).success, "sol set_state reverted");
        let gr = call(&mut gdb, g, encode("get_state()", &[]));
        let sr = call(&mut sdb, s, encode("get_state()", &[]));
        assert_eq!(
            gr.output, sr.output,
            "get_state({}) differs from solidity",
            tag
        );
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(tag),
            "the tag survives a storage round trip"
        );
    }

    assert!(
        call(
            &mut gdb,
            g,
            encode("set_after(uint256)", &[U256::from(12345u64)])
        )
        .success
    );
    let gr = call(&mut gdb, g, encode("get_after()", &[]));
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(12345u64),
        "the field after an enum is not clobbered"
    );
    let gr = call(&mut gdb, g, encode("get_state()", &[]));
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(2u64),
        "and the enum still reads back after its neighbour was written"
    );

    let who: Address = "0x00000000000000000000000000000000cafebabe"
        .parse()
        .unwrap();
    for tag in [0u64, 2, 1] {
        let d = encode_words(
            "set_user(address,uint8)",
            &[word_addr(who), word_u256(U256::from(tag))],
        );
        assert!(
            call(&mut gdb, g, d.clone()).success,
            "gum set_user reverted"
        );
        assert!(call(&mut sdb, s, d).success, "sol set_user reverted");
        let gr = call(
            &mut gdb,
            g,
            encode_words("get_user(address)", &[word_addr(who)]),
        );
        let sr = call(
            &mut sdb,
            s,
            encode_words("get_user(address)", &[word_addr(who)]),
        );
        assert_eq!(
            gr.output, sr.output,
            "get_user({}) differs from solidity",
            tag
        );
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(tag),
            "the tag survives a mapping round trip"
        );
    }

    for tag in [0u64, 1, 2] {
        let d = encode("emit_it(uint8)", &[U256::from(tag)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert!(gr.success, "gum emit_it reverted");
        assert_eq!(gr.logs.len(), 1, "one log");
        assert_eq!(sr.logs.len(), 1, "one log");
        assert_eq!(
            gr.logs[0].0, sr.logs[0].0,
            "topic0 differs: the event signature disagrees with solidity"
        );
        assert_eq!(gr.logs[0].1, sr.logs[0].1, "log data differs from solidity");
        assert_eq!(
            U256::from_be_slice(&gr.logs[0].1),
            U256::from(tag),
            "the log carries the tag, not a pointer"
        );
    }
}

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

    let mut d = selector("total(uint256[][])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&arr);
    let gr = call(&mut gdb, g, d);
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(163u64),
        "1+2+10+20+30+100"
    );

    for (i, want) in [(0u64, 2u64), (1, 3), (2, 1)] {
        let mut d = selector("row_len(uint256[][],uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(64u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.output, sr.output, "row_len({}) differs from solidity", i);
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(want),
            "row_len({}) by value",
            i
        );
    }

    for (i, j, want) in [(0u64, 1u64, 2u64), (1, 2, 30), (2, 0, 100)] {
        let mut d = selector("at(uint256[][],uint256,uint256)").to_vec();
        d.extend_from_slice(&word_u256(U256::from(96u64)));
        d.extend_from_slice(&word_u256(U256::from(i)));
        d.extend_from_slice(&word_u256(U256::from(j)));
        d.extend_from_slice(&arr);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(
            gr.output, sr.output,
            "at({},{}) differs from solidity",
            i, j
        );
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(want),
            "at({},{}) by value",
            i,
            j
        );
    }
}

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
            assert_eq!(
                U256::from_be_slice(&gr.output),
                U256::from($want as u64),
                "{} by value",
                $what
            );
        }};
    }

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
    let g = deploy(
        &mut gdb,
        gum_creation_bytecode(GUM_LOG_NONSCALAR, &solc, true),
    );
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

        assert!(!gr.logs.is_empty(), "{}: no log emitted", what);
    };

    let mut xs: Vec<u8> = word_u256(U256::from(3u64)).to_vec();
    for v in [11u64, 22, 33] {
        xs.extend_from_slice(&word_u256(U256::from(v)));
    }
    let mut d = selector("arr(uint256[])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&xs);
    diff(&mut gdb, &mut sdb, d, "arr");

    let text = b"gum";
    let mut sblob: Vec<u8> = word_u256(U256::from(text.len() as u64)).to_vec();
    let mut padded = text.to_vec();
    padded.resize(32, 0);
    sblob.extend_from_slice(&padded);
    let mut d = selector("str(string)").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&sblob);
    diff(&mut gdb, &mut sdb, d, "str");

    let mut d = selector("tup((uint128,uint256))").to_vec();
    d.extend_from_slice(&word_u256(U256::from(7u64)));
    d.extend_from_slice(&word_u256(U256::from(700u64)));
    diff(&mut gdb, &mut sdb, d, "tup");

    let rows: Vec<Vec<u64>> = vec![vec![1, 2], vec![3]];
    let mut d = selector("grid(uint256[][])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&enc_rows(&rows));
    diff(&mut gdb, &mut sdb, d, "grid");

    let who: Address = "0x00000000000000000000000000000000cafebabe"
        .parse()
        .unwrap();
    let mut d = selector("mixed(address,uint256,uint256[],string)").to_vec();
    d.extend_from_slice(&word_addr(who));
    d.extend_from_slice(&word_u256(U256::from(9u64)));
    d.extend_from_slice(&word_u256(U256::from(128u64)));
    d.extend_from_slice(&word_u256(U256::from(128u64 + 32 + 3 * 32)));
    d.extend_from_slice(&xs);
    d.extend_from_slice(&sblob);
    diff(&mut gdb, &mut sdb, d, "mixed");
}

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

        assert!(
            gr.output.len() > 4,
            "{}: revert data is only a selector",
            what
        );
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
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::ZERO,
            "{} is not zero",
            f
        );
    }

    let key: Address = "0x00000000000000000000000000000000cafebabe"
        .parse()
        .unwrap();
    let r = call(
        &mut db,
        c,
        encode_words(
            "seed(address,uint256)",
            &[word_addr(key), word_u256(U256::from(4242u64))],
        ),
    );
    assert!(r.success, "seed reverted");
    let r = call(
        &mut db,
        c,
        encode_words("struct_zero_after_map(address)", &[word_addr(key)]),
    );
    assert!(r.success, "struct_zero_after_map reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::ZERO,
        "a fresh struct read scratch memory left over from the mapping lookup"
    );

    let r = call(&mut db, c, selector("arr_writable()").to_vec());
    assert!(r.success, "arr_writable reverted");
    assert_eq!(U256::from_be_slice(&r.output), U256::from(9u64), "0 + 9");

    let r = call(
        &mut db,
        c,
        encode_words("delete_struct(address)", &[word_addr(key)]),
    );
    assert!(r.success, "delete_struct reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::ZERO,
        "delete left the struct set"
    );

    let mut d = selector("delete_dyn(uint256[])").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&word_u256(U256::from(2u64)));
    d.extend_from_slice(&word_u256(U256::from(1u64)));
    d.extend_from_slice(&word_u256(U256::from(2u64)));
    let r = call(&mut db, c, d);
    assert!(r.success, "delete_dyn reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::ZERO,
        "delete left the array non-empty"
    );

    let r = call(
        &mut db,
        c,
        selector("delete_leaves_neighbour_alone()").to_vec(),
    );
    assert!(r.success, "delete_leaves_neighbour_alone reverted");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(8u64),
        "delete overran into the next allocation"
    );
}

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

    let d = encode_words(
        "set(uint128,uint256)",
        &[word_u256(U256::from(7u64)), word_u256(U256::from(12345u64))],
    );
    assert!(call(&mut gdb, g, d.clone()).success, "gum set reverted");
    assert!(call(&mut sdb, s, d).success, "solidity set reverted");

    for (f, want) in [("get_b()", 12345u64), ("get_a()", 7)] {
        let gr = call(&mut gdb, g, selector(f).to_vec());
        let sr = call(&mut sdb, s, selector(f).to_vec());
        assert!(gr.success, "gum {} reverted", f);
        assert_eq!(gr.output, sr.output, "{} differs from solidity", f);
        assert_eq!(
            U256::from_be_slice(&gr.output),
            U256::from(want),
            "{} did not persist",
            f
        );
    }

    let d = encode_words("set_tail(uint256)", &[word_u256(U256::from(99u64))]);
    assert!(
        call(&mut gdb, g, d.clone()).success,
        "gum set_tail reverted"
    );
    assert!(call(&mut sdb, s, d).success, "solidity set_tail reverted");
    let gr = call(&mut gdb, g, selector("get_tail()").to_vec());
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(99u64),
        "the field after the struct"
    );

    let gr = call(&mut gdb, g, selector("get_b()").to_vec());
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(12345u64),
        "tail overlapped the struct"
    );
}

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
        let d = encode_words(
            "push_v(uint128,uint256)",
            &[word_u256(U256::from(a)), word_u256(U256::from(b))],
        );
        assert!(call(&mut gdb, g, d.clone()).success, "gum push_v reverted");
        assert!(call(&mut sdb, s, d).success, "solidity push_v reverted");
    }

    let gr = call(&mut gdb, g, selector("v_len()").to_vec());
    let sr = call(&mut sdb, s, selector("v_len()").to_vec());
    assert_eq!(gr.output, sr.output, "v_len differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(2u64),
        "two pushes"
    );

    for (i, wa, wb) in [(0u64, 1u64, 100u64), (1, 2, 200)] {
        for (f, want) in [("v_b(uint256)", wb), ("v_a(uint256)", wa)] {
            let d = encode_words(f, &[word_u256(U256::from(i))]);
            let gr = call(&mut gdb, g, d.clone());
            let sr = call(&mut sdb, s, d);
            assert!(gr.success, "gum {} reverted", f);
            assert_eq!(gr.output, sr.output, "{}[{}] differs from solidity", f, i);
            assert_eq!(
                U256::from_be_slice(&gr.output),
                U256::from(want),
                "{}[{}] by value",
                f,
                i
            );
        }
    }
}

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
        assert_eq!(
            gr.success, sr.success,
            "{} {} {}: success differs (gum={} sol={})",
            f, a, b, gr.success, sr.success
        );
        assert_eq!(
            gr.output, sr.output,
            "{} {} {}: differs from solidity",
            f, a, b
        );
    }

    let d = encode_words("sub8(int8,int8)", &[neg(1), neg(2)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.success, sr.success, "sub8 1-2: success differs");
    assert_eq!(gr.output, sr.output, "sub8 1-2 differs from solidity");
}

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

    let d = encode_words("mul(int256,int256)", &[w(ONE), w(ONE)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum mul reverted");
    assert_eq!(gr.output, sr.output, "1.0 * 1.0 differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(ONE as u128),
        "1.0 * 1.0 is not 1.0"
    );

    for (a, b) in [(ONE * 5 / 2, ONE * 4), (-ONE, ONE * 3), (ONE / 3, ONE * 3)] {
        let d = encode_words("mul(int256,int256)", &[w(a), w(b)]);
        let gr = call(&mut gdb, g, d.clone());
        let sr = call(&mut sdb, s, d);
        assert_eq!(gr.success, sr.success, "mul {} {}: success differs", a, b);
        assert_eq!(
            gr.output, sr.output,
            "mul {} {} differs from solidity",
            a, b
        );
    }

    let d = encode_words("div(int256,int256)", &[w(ONE), w(ONE * 4)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "1.0 / 4.0 differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from((ONE / 4) as u128),
        "1.0 / 4.0 is not 0.25"
    );

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
        assert_eq!(
            gr.output, sr.output,
            "{} {} {} differs from solidity",
            f, a, b
        );
    }

    let d = encode_words("scale(int256)", &[w(ONE * 3)]);
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert_eq!(gr.output, sr.output, "scale differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from((ONE * 6) as u128),
        "3.0 * 2 is not 6.0"
    );

    let d = encode_words("div(int256,int256)", &[w(ONE), w(0)]);
    assert!(!call(&mut gdb, g, d).success, "div by zero did not revert");
}

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

    let r = call(
        &mut db,
        c,
        encode_words("passthrough(address)", &[word_addr(counter)]),
    );
    assert!(r.success, "passthrough reverted");

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

    assert_eq!(
        storage(&mut db, counter, 0),
        U256::from(1u64),
        "name() was called more than once"
    );
}

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
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(10u64),
        "hello+world length"
    );

    let mut d = selector("slice_len(string)").to_vec();
    d.extend_from_slice(&word_u256(U256::from(32u64)));
    d.extend_from_slice(&enc_str(b"abcdef"));
    let gr = call(&mut gdb, g, d.clone());
    let sr = call(&mut sdb, s, d);
    assert!(gr.success, "gum slice_len reverted");
    assert_eq!(gr.output, sr.output, "slice_len differs from solidity");
    assert_eq!(
        U256::from_be_slice(&gr.output),
        U256::from(3u64),
        "slice [1,4) length"
    );
}

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

    fn bump(self) -> u256:
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

    let o1 = both!("parent_bump()");
    assert_eq!(
        U256::from_be_slice(&o1),
        U256::from(1u64),
        "parent version should add 1"
    );
    let o2 = both!("child_bump()");
    assert_eq!(
        U256::from_be_slice(&o2),
        U256::from(101u64),
        "child override should add 100"
    );
    let o3 = both!("parent_bump()");
    assert_eq!(
        U256::from_be_slice(&o3),
        U256::from(102u64),
        "parent version again"
    );
    both!("get_total()");
    assert_eq!(
        storage(&mut gdb, g, 0),
        U256::from(102u64),
        "shared total in slot 0"
    );
}

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
    let word = |u: U256| {
        let mut w = [0u8; 32];
        w.copy_from_slice(&u.to_be_bytes::<32>());
        w
    };

    let negw = |u: U256| word((U256::ZERO).wrapping_sub(u));

    let a = big("200000000000000000000000000000000000000");
    let b = big("300000000000000000000000000000000000000");
    let want = big("60000000000000000000000000000000000000000000000000000000000");
    let mut d = selector("mul(int256,int256)").to_vec();
    d.extend_from_slice(&word(a));
    d.extend_from_slice(&word(b));
    let r = call(&mut db, c, d);
    assert!(
        r.success,
        "mul reverted on a product the scaled result can hold"
    );
    assert_eq!(
        U256::from_be_slice(&r.output),
        want,
        "2e38 * 3e38 (WAD) = 6e58"
    );

    let mut d = selector("mul(int256,int256)").to_vec();
    d.extend_from_slice(&negw(a));
    d.extend_from_slice(&word(b));
    let r = call(&mut db, c, d);
    assert!(r.success, "signed mul reverted");
    assert_eq!(r.output.as_slice(), negw(want), "-2e38 * 3e38 = -6e58");

    let n = big("60000000000000000000000000000000000000000000000000000000000");
    let dd = big("300000000000000000000000000000000000000");
    let dwant = big("200000000000000000000000000000000000000");
    let mut d = selector("div(int256,int256)").to_vec();
    d.extend_from_slice(&word(n));
    d.extend_from_slice(&word(dd));
    let r = call(&mut db, c, d);
    assert!(r.success, "div reverted on a numerator the result can hold");
    assert_eq!(
        U256::from_be_slice(&r.output),
        dwant,
        "6e58 / 3e38 (WAD) = 2e38"
    );

    let huge = big("10000000000000000000000000000000000000000000000000000000000");
    let mut d = selector("mul(int256,int256)").to_vec();
    d.extend_from_slice(&word(huge));
    d.extend_from_slice(&word(huge));
    let r = call(&mut db, c, d);
    assert!(!r.success, "a result exceeding int256 must revert");

    let mut d = selector("div(int256,int256)").to_vec();
    d.extend_from_slice(&word(big("1000000000000000000")));
    d.extend_from_slice(&word(U256::ZERO));
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

    let mut d = selector("bump(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 5;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success);
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(6),
        "mutation must be written back"
    );

    let mut d = selector("bump(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[30] = 0;
        w[31] = 200u8;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success, "the internal revert must be caught");
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(0),
        "caught path returns catch's value"
    );
}

#[test]
fn test_try_catch_captures_param_and_catches_internal_revert() {
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
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(2),
        "success keeps the write"
    );

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
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(1),
        "caught revert must roll back the write"
    );
}

#[test]
fn test_try_catch_captures_a_local_and_catches_internal_revert() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    for decl in ["u256 n = arg", "var n = arg"] {
        let src = include_str!("fixtures/try/local.gum").replace("__DECL__", decl);
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let c = deploy(&mut db, gum_creation_bytecode(&src, &solc, false));

        let mut d = selector("classify(uint256)").to_vec();
        d.extend_from_slice(&{
            let mut w = [0u8; 32];
            w[31] = 5;
            w
        });
        let r = call(&mut db, c, d);
        assert!(r.success, "[{}] classify(5) reverted", decl);
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(5),
            "[{}] captured local should reach the body",
            decl
        );
        let r = call(&mut db, c, selector("getmark()").to_vec());
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(2),
            "[{}] success keeps the write",
            decl
        );

        let mut d = selector("classify(uint256)").to_vec();
        d.extend_from_slice(&{
            let mut w = [0u8; 32];
            w[31] = 20;
            w
        });
        let r = call(&mut db, c, d);
        assert!(
            r.success,
            "[{}] internal revert must be caught with a captured local, not bubble out",
            decl
        );
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(99),
            "[{}] catch should return 99",
            decl
        );
        let r = call(&mut db, c, selector("getmark()").to_vec());
        assert_eq!(
            U256::from_be_slice(&r.output),
            U256::from(1),
            "[{}] caught revert must roll back the write",
            decl
        );
    }
}

#[test]
fn test_try_catch_writes_back_a_string_of_any_type() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/try/string_writeback.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    let mut d = selector("label(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 3;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success, "label(3) reverted");
    assert_eq!(
        r.output,
        abi_encode_string(b"updated-to-a-long-value-past-31-bytes-so-it-goes-long-form"),
        "success must write back the mutated String"
    );

    let mut d = selector("label(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 20;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success, "internal revert must be caught");
    assert_eq!(
        r.output,
        abi_encode_string(b"caught"),
        "caught path returns the catch value"
    );
}

#[test]
fn test_nested_try_catches_at_each_level() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let src = include_str!("fixtures/try/nested.gum");
    let mut db: Db = CacheDB::new(EmptyDB::default());
    let c = deploy(&mut db, gum_creation_bytecode(src, &solc, false));

    let mut d = selector("f(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 2;
        w
    });
    let r = call(&mut db, c, d);
    assert!(r.success);
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(1),
        "inner success path"
    );

    let mut d = selector("f(uint256)").to_vec();
    d.extend_from_slice(&{
        let mut w = [0u8; 32];
        w[31] = 9;
        w
    });
    let r = call(&mut db, c, d);
    assert!(
        r.success,
        "inner internal revert must be caught by the inner catch"
    );
    assert_eq!(
        U256::from_be_slice(&r.output),
        U256::from(2),
        "inner catch path"
    );
}

#[test]
fn narrow_arithmetic_overflow_is_independent_of_literal_position() {
    let solc = match solc_path() {
        Some(p) => p,
        None => return,
    };
    let gum = gum_creation_bytecode(include_str!("fixtures/arith/literal_pos.gum"), &solc, false);
    let sol = sol_creation_bytecode(include_str!("fixtures/arith/literal_pos.sol"), &solc);
    let mut gdb: Db = CacheDB::new(EmptyDB::default());
    let mut sdb: Db = CacheDB::new(EmptyDB::default());
    let ga = deploy(&mut gdb, gum);
    let sa = deploy(&mut sdb, sol);
    let w = |v: u64| {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&v.to_be_bytes());
        b
    };

    assert!(call(&mut gdb, ga, encode_words("seta(uint256)", &[w(10)])).success);
    assert!(call(&mut sdb, sa, encode_words("seta(uint256)", &[w(10)])).success);
    for sig in ["a_lit()", "a_var(uint256)"] {
        let args = if sig == "a_lit()" { vec![] } else { vec![w(2)] };
        let g = call(&mut gdb, ga, encode_words(sig, &args));
        let s = call(&mut sdb, sa, encode_words(sig, &args));
        assert!(g.success && s.success, "{} reverted", sig);
        assert_eq!(g.output, s.output, "{} differs", sig);
        assert_eq!(
            U256::from_be_slice(&g.output),
            U256::from(20u64),
            "{} wrong",
            sig
        );
    }

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
        assert_eq!(
            g.success, s.success,
            "{}: revert must agree with Solidity",
            sig
        );
        assert!(!g.success, "{}: narrow overflow must revert", sig);
    }
}
