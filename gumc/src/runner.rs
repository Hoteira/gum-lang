use revm::context::result::{ExecutionResult, Output};
use revm::context::TxEnv;
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::{hardfork::SpecId, Address, TxKind, U256};
use revm::{Context, ExecuteCommitEvm, MainBuilder, MainContext};

type Db = CacheDB<EmptyDB>;

// Under Osaka's per-transaction gas cap (EIP-7825, 2^24).
const GAS_LIMIT: u64 = 16_000_000;

fn tester() -> Address {
    Address::from([0x11u8; 20])
}

fn selector(sig: &str) -> [u8; 4] {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(sig.as_bytes());
    k.finalize(&mut out);
    [out[0], out[1], out[2], out[3]]
}

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

fn deploy(db: &mut Db, creation: Vec<u8>) -> Result<Address, String> {
    let mut evm = evm_for!(db);
    let tx = TxEnv::builder()
        .caller(tester())
        .kind(TxKind::Create)
        .data(creation.into())
        .value(U256::ZERO)
        .gas_limit(GAS_LIMIT)
        .build()
        .map_err(|e| format!("{:?}", e))?;
    match evm.transact_commit(tx).map_err(|e| format!("{:?}", e))? {
        ExecutionResult::Success { output: Output::Create(_, Some(addr)), .. } => Ok(addr),
        other => Err(format!(
            "the contract did not deploy: {:?}. A test contract must be deployable with no constructor arguments.",
            other
        )),
    }
}

fn call(db: &mut Db, to: Address, data: Vec<u8>) -> (bool, Vec<u8>) {
    let mut evm = evm_for!(db);
    let tx = TxEnv::builder()
        .caller(tester())
        .kind(TxKind::Call(to))
        .data(data.into())
        .value(U256::ZERO)
        .gas_limit(GAS_LIMIT)
        .build()
        .expect("bad call tx");
    match evm.transact_commit(tx).expect("call tx failed") {
        ExecutionResult::Success { output, .. } => (true, output.into_data().to_vec()),
        ExecutionResult::Revert { output, .. } => (false, output.to_vec()),
        ExecutionResult::Halt { .. } => (false, vec![]),
    }
}

// Turns raw revert data into something a person can read: a require/assert
// string (Error(string)), a Panic(uint256) code, or the bare selector of a
// custom error when that is all there is.
fn decode_revert(out: &[u8]) -> String {
    if out.is_empty() {
        return "reverted with no reason".to_string();
    }
    if out.len() >= 68 && out[..4] == [0x08, 0xc3, 0x79, 0xa0] {
        // Error(string): selector, offset, length, bytes.
        let len = usize::from_be_bytes(out[60..68].try_into().unwrap_or([0u8; 8]));
        if out.len() >= 68 + len {
            return format!("\"{}\"", String::from_utf8_lossy(&out[68..68 + len]));
        }
    }
    if out.len() >= 36 && out[..4] == [0x4e, 0x48, 0x7b, 0x71] {
        // Panic(uint256): the code sits in the low byte of the word.
        return format!("Panic(0x{:02x})", out[35]);
    }
    format!("reverted with custom error 0x{}", hex::encode(&out[..4.min(out.len())]))
}

pub struct TestOutcome {
    pub name: String,
    pub passed: bool,
    pub reason: Option<String>,
}

// Runs every named test against a fresh deployment of bytecode and returns
// one outcome per test. A test is the no-argument export name; it is invoked
// as name().
pub fn run_contract_tests(bytecode: &[u8], test_fns: &[String]) -> Result<Vec<TestOutcome>, String> {
    let mut outcomes = Vec::with_capacity(test_fns.len());
    for name in test_fns {
        let mut db: Db = CacheDB::new(EmptyDB::default());
        let addr = deploy(&mut db, bytecode.to_vec())?;
        let data = selector(&format!("{}()", name)).to_vec();
        let (passed, ret) = call(&mut db, addr, data);
        outcomes.push(TestOutcome {
            name: name.clone(),
            passed,
            reason: if passed { None } else { Some(decode_revert(&ret)) },
        });
    }
    Ok(outcomes)
}
