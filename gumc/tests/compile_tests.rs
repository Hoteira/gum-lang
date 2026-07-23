use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

// Same lookup as the execution tests: $SOLC, then tools/solc(.exe), then PATH.
// GUM_REQUIRE_SOLC=1 makes a missing solc an error rather than a skip, so CI cannot go green while quietly asserting nothing.
fn find_solc() -> Option<PathBuf> {
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

fn repo_root() -> PathBuf {
    // gumc/tests/ -> gumc/ -> gum/ (where std/ and examples/ live)
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

// Writes source to a uniquely-named temp file, runs it through the real
// gumc binary, and returns (succeeded, combined stdout+stderr).
fn run_gumc(source: &str) -> (bool, String) {
    run_gumc_with_args(source, &[])
}

fn run_gumc_with_args(source: &str, extra_args: &[&str]) -> (bool, String) {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut path = std::env::temp_dir();
    path.push(format!("gumc_test_{}_{}.gum", std::process::id(), id));
    std::fs::write(&path, source).expect("failed to write temp .gum file");

    let output = Command::new(env!("CARGO_BIN_EXE_gumc"))
        .arg(&path)
        .args(extra_args)
        .output()
        .expect("failed to run gumc binary");

    let _ = std::fs::remove_file(&path);

    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

// Asserts the source compiles all the way down to EVM bytecode via
// --bytecode. Doubles as a Yul validity check: gumc itself never parses
// the Yul it emits, so solc's strict-assembly front-end is the only thing
fn assert_assembles(source: &str) {
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping bytecode assertion: no solc found");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let (ok, output) = run_gumc_with_args(source, &["--bytecode", "--solc", &solc_arg]);
    assert!(ok, "expected bytecode assembly to succeed, got:\n{}", output);
    assert!(
        output.contains("EVM bytecode") && output.contains("0x"),
        "expected bytecode hex in output, got:\n{}",
        output
    );
}

fn assert_compiles(source: &str) {
    let (ok, output) = run_gumc(source);
    assert!(ok, "expected successful compile, got:\n{}", output);
}

fn assert_compile_fails(source: &str) {
    let (ok, output) = run_gumc(source);
    assert!(!ok, "expected compile failure, but it succeeded:\n{}", output);
}

fn assert_output_contains(source: &str, needle: &str) {
    let (ok, output) = run_gumc(source);
    assert!(ok, "expected successful compile, got:\n{}", output);
    assert!(
        output.contains(needle),
        "expected output to contain {:?}, got:\n{}",
        needle,
        output
    );
}

// Asserts the output contains prefix<digits>suffix. Codegen names thunks
// with a shared counter (log_ptr_3, __strlit_7, …) whose value shifts
// whenever unrelated codegen changes, so tests that care about the shape of
fn assert_output_contains_numbered(source: &str, prefix: &str, suffix: &str) {
    let (ok, output) = run_gumc(source);
    assert!(ok, "expected successful compile, got:\n{}", output);
    let found = output.match_indices(prefix).any(|(i, _)| {
        let rest = &output[i + prefix.len()..];
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        digits > 0 && rest[digits..].starts_with(suffix)
    });
    assert!(
        found,
        "expected output to contain {:?}<digits>{:?}, got:\n{}",
        prefix, suffix, output
    );
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

// --- Real contracts and stdlib ---

#[test]
fn token_gum_compiles() {
    assert_compiles(&read_repo_file("examples/token.gum"));
}

#[test]
fn amm_gum_compiles() {
    assert_compiles(&read_repo_file("examples/amm.gum"));
}

// The banner names the temp file gumc was handed, which is unique per run, so it is dropped before comparing. Everything after it is the Yul.
fn yul_only(out: &str) -> String {
    out.lines().filter(|l| !l.starts_with("--> Compiling")).collect::<Vec<_>>().join("\n")
}

#[test]
fn compiling_the_same_source_twice_gives_the_same_output() {
    // A build has to be reproducible or a deployed contract cannot be verified against its source.
    // Two HashMaps decided emission order (the helper thunks, and the class-method loop), and Rust randomizes HashMap iteration per process, so every compile produced different but equivalent bytecode. Separate processes here, because that is where the randomness lives.
    let src = read_repo_file("examples/amm.gum");
    let (ok, first) = run_gumc(&src);
    assert!(ok, "amm failed to compile:\n{}", first);
    let first = yul_only(&first);
    for i in 0..3 {
        let (ok2, again) = run_gumc(&src);
        assert!(ok2, "amm failed to compile on run {}", i);
        assert_eq!(first, yul_only(&again), "compile {} differed from the first: output is not reproducible", i);
    }
}

#[test]
fn the_standard_library_module_compiles() {
    // The module gumc embeds, compiled through gumc itself. include_str! is the same path stdlib.rs uses, so this cannot pass against a file the binary does not ship.
    let source = include_str!("../../std/defaults.gum");
    let (ok, output) = run_gumc(source);
    assert!(ok, "std/defaults.gum failed to compile:\n{}", output);
}

#[test]
fn every_standard_symbol_imports_on_its_own() {
    // Each symbol has to drag in its own dependencies: Account needs Serializable, HashMap needs Hashable. Importing one at a time is what proves the closure is complete.
    for sym in [
        "Account", "Message", "Block", "String", "Bytes", "HashMap", "Vec",
        "Serializable", "Hashable", "Crypto", "keccak256", "ecrecover",
    ] {
        let src = format!("use gum.defaults.{}\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n", sym);
        let (ok, output) = run_gumc(&src);
        assert!(ok, "use gum.defaults.{} failed:\n{}", sym, output);
    }
}

// --- Language features ---

#[test]
fn operator_precedence_groups_comparison_below_logical_or() {
    // Regression for the flat-fold bug: a == b || c == d must group as
    // or(eq(a,b), eq(c,d)), NOT the left-to-right eq(or(eq(a,b),c),d)
    // that silently made amm.swap always revert.
    assert_output_contains(
        "contract App:\n    export fn f(u256 a, u256 b, u256 c, u256 d) -> bool:\n        return a == b || c == d\n",
        "or(eq(a, b), eq(c, d))",
    );
}

#[test]
fn arithmetic_binds_tighter_than_comparison() {
    // a + b < c must be lt(add..., c), not add(a, lt(b, c)).
    assert_output_contains(
        "contract App:\n    export fn f(u256 a, u256 b, u256 c) -> bool:\n        return a + b < c\n",
        "lt(checked_add(a, b, not(0)), c)",
    );
}

#[test]
fn account_param_abi_encodes_as_address() {
    // Account must ABI-encode as address (not a (uint256) tuple) so its
    // selector matches standard callers. Checks the emitted ABI JSON.
    assert_output_contains(
        "use gum.defaults.Account\n\ncontract App:\n    export fn take(Account who) -> u256:\n        return 0\n",
        "\"address\"",
    );
}

#[test]
fn narrow_int_literal_is_masked() {
    // u8 x = 5 must actually be masked, not just accepted, see mask_for_type.
    assert_output_contains(
        "contract App:\n    export fn foo() -> u8:\n        u8 x = 5\n        return x\n",
        "and(5, 0xff)",
    );
}

#[test]
fn signed_negation_uses_signextend_not_a_bitmask() {
    assert_output_contains(
        "contract App:\n    export fn foo() -> i8:\n        i8 y = -3\n        return y\n",
        "signextend(0, sub(0, 3))",
    );
}

#[test]
fn unary_not_compiles_to_iszero() {
    assert_output_contains(
        "contract App:\n    export fn foo(bool flag) -> bool:\n        return !flag\n",
        "iszero(flag)",
    );
}

#[test]
fn modulo_is_checked_and_signed_aware() {
    assert_output_contains(
        "contract App:\n    export fn foo(u256 a, u256 b) -> u256:\n        return a % b\n",
        "checked_mod(a, b)",
    );
}

#[test]
fn power_uses_native_exp_opcode() {
    assert_output_contains(
        "contract App:\n    export fn foo(u256 a, u256 b) -> u256:\n        return a ** b\n",
        "exp(a, b)",
    );
}

#[test]
fn array_literal_and_indexing_compile() {
    assert_output_contains(
        "contract App:\n    export fn foo() -> u256:\n        mut [u256; 3] xs = [1, 2, 3]\n        return xs[1]\n",
        "__arrlit_",
    );
}

#[test]
fn u8_array_is_packed_tightly_not_32_bytes_per_element() {
    let (ok, output) = run_gumc(
        "contract App:\n    export fn foo() -> u256:\n        mut [u8; 4] xs = [1, 2, 3, 4]\n        mut u256 total = 0\n        for x in xs:\n            total = total + x\n        return total\n",
    );
    assert!(ok, "expected successful compile, got:\n{}", output);
    assert!(
        output.contains("allocate_memory(4)"),
        "expected a 4-byte allocation (1 byte/element), got:\n{}",
        output
    );
}

#[test]
fn compound_assignment_desugars_to_checked_arithmetic() {
    assert_output_contains(
        "contract App:\n    export fn foo(u256 x) -> u256:\n        mut u256 total = x\n        total += 5\n        return total\n",
        "checked_add(total, 5",
    );
}

#[test]
fn constructor_call_resolves_correctly() {
    // Regression test for the type_ident grammar fix: new Counter(start)
    // used to be misparsed as instantiating a generic class.
    assert_output_contains(
        "class Counter:\n    u256 value\n\n    fn new(u256 start):\n        self.value = start\n\n    fn get() -> u256:\n        return self.value\n\ncontract App:\n    export fn make(u256 start) -> u256:\n        mut Counter c = new Counter(start)\n        return c.get()\n",
        "__new_Counter",
    );
}

#[test]
fn cheatcode_sender_changes_msg_sender() {
    // Vm.sender = addr makes following calls come from addr. The first test
    // asserts the set sender is seen; the second (unset) asserts it is not, so
    // the first can't pass by accident.
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping sender check: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let src = "use gum.defaults.hashable\nuse gum.defaults.Message\n\ncontract Target:\n    Account seen\n    export fn record():\n        Target.seen = Message.sender()\n    export fn who() -> Account:\n        return Target.seen\n\ninterface ITarget:\n    fn record()\n    fn who() -> Account\n\ncontract SenderTest:\n    [Test]\n    fn sender_sets_msg_sender():\n        var t = new Target()\n        Vm.sender = 0x00000000000000000000000000000000000000AA\n        ITarget(t).record()\n        assert(ITarget(t).who() == 0x00000000000000000000000000000000000000AA, \"sender not set\")\n    [Test]\n    fn without_set_sender_differs():\n        var t = new Target()\n        ITarget(t).record()\n        assert(ITarget(t).who() != 0x00000000000000000000000000000000000000AA, \"unexpected sender\")\n";
    let (ok, output) = run_gumc_with_args(src, &["--test", "--solc", &solc_arg]);
    assert!(ok, "sender tests should pass:\n{}", output);
    assert!(output.contains("ok    sender_sets_msg_sender"), "Vm.sender did not take effect:\n{}", output);
    assert!(output.contains("2 tests, 2 passed"), "expected both to pass:\n{}", output);
}

#[test]
fn scoped_try_catches_internal_revert_and_rolls_back() {
    // A self-contained try body runs in its own call frame, so an internal
    // revert (here assert(false)) is caught (Solidity's try/catch cannot) and
    // the body's storage write rolls back before catch runs. Proof: x is set to
    // 1, the try sets it to 2 then reverts, and after catch (which adds 10) x is
    // 11 -- not 12, which is only possible if the write to 2 rolled back.
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping scoped-try check: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let src = "contract C:\n    u256 x\n\n    [Test]\n    fn catches_and_rolls_back():\n        C.x = 1\n        try:\n            C.x = 2\n            assert(false, \"boom\")\n        catch:\n            C.x = C.x + 10\n        assert(C.x == 11, \"expected rollback then catch bump\")\n\n    [Test]\n    fn success_keeps_write():\n        C.x = 5\n        try:\n            C.x = 7\n        catch:\n            C.x = 99\n        assert(C.x == 7, \"success path must keep the write\")\n";
    let (ok, output) = run_gumc_with_args(src, &["--test", "--solc", &solc_arg]);
    assert!(ok, "scoped-try tests should pass:\n{}", output);
    assert!(output.contains("2 tests, 2 passed"), "expected both to pass:\n{}", output);
}

#[test]
fn test_runner_reports_pass_and_fail() {
    // gumc --test deploys the contract and runs every no-arg export test().
    // A passing test returns; a failing one reverts, and its reason is shown.
    // Any failure makes the process exit non-zero, so CI can gate on it.
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test runner check: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    // A [Test] fn is a test; a plain fn beside it is a helper and is not run.
    let src = "use gum.defaults.hashable\n\ncontract Suite:\n    fn helper() -> u256:\n        return 1\n\n    [Test]\n    fn passes():\n        assert(self.helper() == 1, \"nope\")\n\n    [Test]\n    fn fails():\n        assert(1 == 2, \"boom\")\n";
    let (ok, output) = run_gumc_with_args(src, &["--test", "--solc", &solc_arg]);
    assert!(!ok, "a failing test must make gumc exit non-zero:\n{}", output);
    assert!(output.contains("ok    passes"), "expected a pass line:\n{}", output);
    assert!(output.contains("FAIL  fails"), "expected a fail line:\n{}", output);
    assert!(output.contains("\"boom\""), "expected the revert reason:\n{}", output);
    assert!(output.contains("1 passed, 1 failed"), "expected a summary:\n{}", output);
    assert!(!output.contains("helper"), "a plain fn helper must not run as a test:\n{}", output);
}

#[test]
fn discarded_call_return_is_popped() {
    // A method call used as a statement discards its return value. Yul rejects a
    // top-level expression that returns a value, so codegen must pop() it. This
    // is exactly OZ ERC721's _requireOwned(tokenId); for-its-revert idiom.
    let source = "contract C:\n    u256 x\n\n    fn side() -> u256:\n        self.x = 1\n        return self.x\n\n    fn go():\n        self.x = 2\n\n    export fn run():\n        self.side()\n        self.go()\n";
    assert_output_contains(source, "pop(C_side())");
    assert_assembles(source);
}

#[test]
fn constructor_emits_abi_and_assembles() {
    let source = "contract Token:\n    u256 supply\n\n    fn new(u256 s):\n        self.supply = s\n\n    export fn dummy() -> u256:\n        return 0\n";
    assert_output_contains(source, "\"type\": \"constructor\"");
    assert_assembles(source);
}

#[test]
fn vec_push_and_get_monomorphize_per_instantiation() {
    assert_output_contains(
        "use gum.defaults.vec\n\ncontract App:\n    export fn foo() -> u256:\n        mut Vec(u256) v = new Vec(u256)()\n        v = v.push(10)\n        v = v.push(20)\n        return v.get(1)\n",
        "Vec_u256_push",
    );
}

#[test]
fn hashmap_get_set_use_the_field_storage_slot() {
    assert_output_contains(
        "use gum.defaults.hashable\n\ncontract Registry:\n    HashMap(Account, u256) scores\n\n    export fn set_score(Account who, u256 score):\n        Registry.scores.set(who, score)\n",
        "gum_hash_slot",
    );
}

// --- Safety-hardening codegen (guards emitted leanly, once) ---

#[test]
fn nonpayable_callvalue_guard_is_hoisted_once() {
    // Exactly one callvalue guard for a two-function contract, and it sits
    // before the selector switch (dispatcher entry), not inside each case.
    let (ok, output) = run_gumc(
        "contract App:\n    export fn a(u256 x) -> u256:\n        return x\n\n    export fn b(u256 y) -> u256:\n        return y\n",
    );
    assert!(ok, "{}", output);
    let guards = output.matches("if callvalue()").count();
    assert_eq!(guards, 1, "expected a single hoisted callvalue guard, got {}:\n{}", guards, output);
}

#[test]
fn account_params_are_masked_to_160_bits() {
    assert_output_contains(
        "use gum.defaults.Account\n\ncontract App:\n    export fn who(Account a) -> u256:\n        return 0\n",
        "and(calldataload(4), 0xffffffffffffffffffffffffffffffffffffffff)",
    );
}

#[test]
fn external_calls_validate_returndata_size() {
    assert_output_contains(
        &read_repo_file("examples/amm.gum"),
        "if lt(returndatasize(), 32) { revert(0, 0) }",
    );
}

#[test]
fn constant_arithmetic_is_folded_not_checked() {
    let (ok, output) = run_gumc(
        "contract App:\n    export fn f() -> u256:\n        u256 x = 3 + 4\n        return x\n",
    );
    assert!(ok, "{}", output);
    assert!(output.contains(":= 7"), "expected 3+4 folded to 7:\n{}", output);
    assert!(!output.contains("checked_add(3, 4"), "constant add should not emit a runtime check:\n{}", output);
}

#[test]
fn unprovable_constant_overflow_keeps_the_runtime_check() {
    // u128::MAX  2 overflows the u128 folding accumulator, so const_fold
    // must bail to the checked runtime path rather than emit a wrong literal.
    let (ok, output) = run_gumc(
        "contract App:\n    export fn f() -> u256:\n        u256 x = 340282366920938463463374607431768211455 * 2\n        return x\n",
    );
    assert!(ok, "{}", output);
    assert!(
        output.contains("checked_mul(340282366920938463463374607431768211455, 2"),
        "expected the check to survive an unprovable fold:\n{}",
        output
    );
}

// --- Revert richness and dispatcher guards ---

#[test]
fn calldatasize_guard_is_present_once() {
    let (ok, output) = run_gumc("contract App:\n    export fn a(u256 x) -> u256:\n        return x\n");
    assert!(ok, "{}", output);
    assert!(output.contains("if lt(calldatasize(), 4)"), "missing calldatasize guard:\n{}", output);
}

#[test]
fn default_reverts_are_blank() {
    // Without --rich-reverts, checked arithmetic reverts with zero data and
    // never emits the Panic selector.
    let (ok, output) = run_gumc("contract App:\n    export fn f(u256 a, u256 b) -> u256:\n        return a + b\n");
    assert!(ok, "{}", output);
    assert!(!output.contains("4e487b71"), "default build must not carry Panic data:\n{}", output);
}

#[test]
fn rich_reverts_flag_emits_panic_codes() {
    // With --rich-reverts, overflow -> Panic(0x11), div-by-zero -> Panic(0x12).
    let (ok, output) = run_gumc_with_args(
        "contract App:\n    export fn f(u256 a, u256 b) -> u256:\n        return a / b\n",
        &["--rich-reverts"],
    );
    assert!(ok, "{}", output);
    assert!(output.contains("mstore(0, shl(224, 0x4e487b71))"), "expected Panic selector:\n{}", output);
    assert!(output.contains("mstore(4, 0x12)"), "expected div-by-zero panic code 0x12:\n{}", output);
}

// --- Indexed event logging (Level 2) ---

#[test]
fn indexed_event_uses_canonical_erc20_transfer_topic() {
    // topic0 must be keccak256("Transfer(address,address,uint256)"), the
    // real ERC20 Transfer hash every wallet/indexer keys on, proving the
    // signature is built from arg types (Account->address, u256->uint256),
    // and that indexed() args route to LOG topics (log3), not data.
    let src = read_repo_file("examples/token.gum");
    assert_output_contains(
        &src,
        "log3(blob, alen, 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef, sender, to)",
    );
    // Two indexed fields leave one word of data, so the encoder's head is 32 and there is no tail.
    assert_output_contains(&src, "let alen := 32");
}

#[test]
fn non_indexed_event_stays_in_data() {
    // Mint(to indexed, value): one indexed topic -> log2, value in 32B data.
    let src = read_repo_file("examples/token.gum");
    assert_output_contains(&src, "log2(blob, alen,");
    assert_output_contains(&src, "let alen := 32");
}

#[test]
fn events_still_assemble_to_bytecode() {
    assert_assembles(&read_repo_file("examples/token.gum"));
    assert_assembles(&read_repo_file("examples/amm.gum"));
}

// --- Bytecode assembly (Yul -> EVM via solc) ---

#[test]
fn token_gum_assembles_to_bytecode() {
    assert_assembles(&read_repo_file("examples/token.gum"));
}

#[test]
fn amm_gum_assembles_to_bytecode() {
    assert_assembles(&read_repo_file("examples/amm.gum"));
}

#[test]
fn feature_mix_assembles_to_bytecode() {
    // One snippet touching the session's riskiest codegen paths, packed
    // narrow-int arrays, for-loops, compound assignment, checked arithmetic,
    // signed ops, and monomorphized Vec, so solc validates that Yul too.
    assert_assembles(
        "use gum.defaults.vec\n\ncontract App:\n    export fn crunch(u256 seed) -> u256:\n        mut [u8; 4] xs = [1, 2, 3, 4]\n        mut u256 total = seed\n        for x in xs:\n            total += x\n        mut i256 signed = -3\n        mut Vec(u256) v = new Vec(u256)()\n        v = v.push(total)\n        return v.get(0) % 7 + total ** 2\n",
    );
}

// --- Error reporting ---

#[test]
fn multiple_independent_semantic_errors_are_all_reported() {
    let (ok, output) = run_gumc(
        "contract App:\n    export fn broken_one() -> u256:\n        return missing_one\n\n    export fn broken_two() -> u256:\n        return missing_two\n",
    );
    assert!(!ok);
    assert!(output.contains("missing_one"), "missing first error:\n{}", output);
    assert!(output.contains("missing_two"), "missing second error:\n{}", output);
    assert!(output.contains("2 semantic error"), "expected an error count summary:\n{}", output);
}

#[test]
fn semantic_errors_carry_exact_line_and_col() {
    // The undefined identifier sits on source line 11, with return starting
    // at column 9 (two levels of indent inside the contract). This guards both
    // the indent preprocessor's line fidelity (closers must not push later
    // lines down) and the col computation (indentation must not be doubled).
    let (ok, output) = run_gumc(
        "\n// comment line\n\ncontract App:\n    export fn fine() -> u256:\n        if 1 > 0:\n            return 1\n        return 2\n\n    export fn broken() -> u256:\n        return nope\n",
    );
    assert!(!ok);
    assert!(
        output.contains("at 11:9"),
        "expected error located at 11:9, got:\n{}",
        output
    );
}

#[test]
fn type_mismatch_is_rejected() {
    assert_compile_fails("contract App:\n    export fn foo() -> u256:\n        bool b = 5\n        return 1\n");
}

#[test]
fn undefined_identifier_is_rejected() {
    assert_compile_fails("contract App:\n    export fn foo() -> u256:\n        return not_a_real_thing\n");
}

#[test]
fn missing_return_on_all_paths_is_rejected() {
    assert_compile_fails(
        "contract App:\n    export fn foo(bool flag) -> u256:\n        if flag:\n            return 1\n",
    );
}

// --- Friendly keyword set: contract / interface / export / var ---

#[test]
fn friendly_keywords_compile_and_assemble() {
    // contract, interface, export
    // fn) and var (inferred local) all in one contract.
    let src = "\
use gum.defaults.Account
use gum.defaults.Message

interface IERC20:
    fn transfer(Account to, u256 amount) -> bool

contract Bank:
    Account owner
    u256 total

    fn new(u256 seed):
        Bank.owner = Message.sender()
        Bank.total = seed

    export fn deposit(u256 amount):
        var current = Bank.total
        Bank.total = current + amount

    export fn total_of() -> u256:
        return Bank.total
";
    assert_compiles(src);
    assert_assembles(src);
}

#[test]
fn var_infers_from_initializer() {
    // var picks up the initializer's type; assigning a bool result to it and
    // returning it as bool must type-check.
    assert_compiles("contract App:\n    export fn f() -> bool:\n        var x = 1 == 1\n        return x\n");
}

#[test]
fn var_without_initializer_is_rejected() {
    // The inferred form has nothing to infer from without an initializer; the
    // grammar requires =, so this is a parse failure.
    assert_compile_fails("contract App:\n    export fn f():\n        var x\n");
}

#[test]
fn const_infers_and_is_immutable() {
    assert_compiles("contract App:\n    export fn f() -> u256:\n        const x = 40 + 2\n        return x\n");
    assert_compile_fails("contract App:\n    export fn f() -> u256:\n        const x = 42\n        x = 43\n        return x\n");
}

#[test]
fn var_is_immutable_by_default_mut_opts_in() {
    // A bare var is immutable, reassigning it is an error.
    assert_compile_fails("contract App:\n    export fn f() -> u256:\n        var x = 42\n        x = 43\n        return x\n");
    // mut var makes it reassignable.
    assert_compiles("contract App:\n    export fn f() -> u256:\n        mut var x = 42\n        x = 43\n        return x\n");
}

#[test]
fn global_keyword_is_gone() {
    // global was removed in favor of contract/export; it no longer parses.
    assert_compile_fails("global fn f() -> u256:\n    return 1\n");
    assert_compile_fails("global class S:\n    u256 a\n");
}

#[test]
fn bare_fn_is_not_externally_callable() {
    // An internal (non-export) fn gets no ABI entry / dispatcher selector.
    // (gumc prints the generated ABI JSON on a normal compile.)
    let (ok, out) = run_gumc(
        "fn helper(u256 x) -> u256:\n    return x + 1\n\ncontract App:\n    export fn pub() -> u256:\n        return helper(1)\n",
    );
    assert!(ok, "expected compile success:\n{}", out);
    assert!(!out.contains("\"helper\""), "internal fn must not appear in the ABI:\n{}", out);
    assert!(out.contains("\"pub\""), "exported fn must appear in the ABI:\n{}", out);
}

#[test]
fn payable_is_marked_in_the_abi() {
    let (ok, out) = run_gumc(
        "contract S:
    u256 t

    export payable fn deposit():
        S.t = 1

    export fn plain():
        S.t = 2
",
    );
    assert!(ok, "expected successful compile, got:
{}", out);
    assert!(out.contains("\"payable\""), "payable fn must be marked payable in the ABI:
{}", out);
    assert!(out.contains("\"nonpayable\""), "non-payable fn must stay nonpayable in the ABI:
{}", out);
}

#[test]
fn nonpayable_guard_is_hoisted_only_when_nothing_is_payable() {
    // No payable fn: one hoisted guard at the dispatcher entry, no per-case copies.
    let (ok, out) = run_gumc("contract S:
    u256 t

    export fn a():
        S.t = 1

    export fn b():
        S.t = 2
");
    assert!(ok, "expected successful compile, got:
{}", out);
    assert_eq!(
        out.matches("if callvalue() { revert(0, 0) }").count(),
        1,
        "expected exactly one hoisted callvalue guard, got:
{}",
        out
    );

    // With a payable fn the hoist is unsound, so each non-payable case guards
    // itself: 2 non-payable functions -> 2 guards, and none before the switch.
    let (ok, out) = run_gumc(
        "contract S:
    u256 t

    export payable fn p():
        S.t = 3

    export fn a():
        S.t = 1

    export fn b():
        S.t = 2
",
    );
    assert!(ok, "expected successful compile, got:
{}", out);
    assert_eq!(
        out.matches("if callvalue() { revert(0, 0) }").count(),
        2,
        "expected one callvalue guard per non-payable case, got:
{}",
        out
    );
}

#[test]
fn bare_return_is_allowed_only_in_void_functions() {
    // A function with no declared return type may return bare to exit early.
    assert_compiles("contract App:\n    export fn f(u256 x):\n        if x == 0:\n            return\n        return\n");
    // One that promises a value must supply it.
    assert_compile_fails("contract App:\n    export fn f() -> u256:\n        return\n");
    // ...and one that promises nothing must not return a value, that would
    // ABI-encode 32 bytes of output its own ABI says don't exist.
    assert_compile_fails("contract App:\n    export fn f():\n        return 5\n");
}

#[test]
fn reentrancy_guard_sees_calls_the_semantic_pass_could_have_missed() {
    // Both of these once produced ZERO guards, a silently reentrant contract.
    // An external call hidden in an if condition (the condition used to be
    // discarded by a .. pattern and never evaluated)...
    assert_output_contains(
        "use gum.defaults.Account\ninterface IERC20:\n    fn transfer(Account to, u256 amount) -> bool\n\ncontract S:\n    u256 t\n\n    export fn go(Account tok):\n        if IERC20(tok).transfer(tok, 1):\n            S.t = 1\n",
        "tload",
    );
    // ...and a raw CALL inside an opaque unsafe Yul block.
    assert_output_contains(
        "contract S:\n    u256 t\n\n    export fn go():\n        S.t = 1\n        unsafe:\n            let ok := call(gas(), 0x1234, 0, 0, 0, 0, 0)\n",
        "tload",
    );
}

#[test]
fn no_reentrancy_guard_when_nothing_can_yield() {
    // A contract that never hands control to another address cannot be
    // re-entered, so it pays nothing for the guard.
    //
    let (ok, out) = run_gumc("contract S:\n    u256 t\n\n    export fn a():\n        S.t = 1\n\n    export fn b() -> u256:\n        return S.t\n");
    assert!(ok, "expected successful compile, got:\n{}", out);
    assert!(!out.contains("tload(0x"), "a contract with no external calls must emit no guard:\n{}", out);
}

#[test]
fn a_persistent_only_contract_has_no_transient_opcodes_in_its_bytecode() {
    // The storage kind is resolved in Rust, and each helper is emitted once per
    // kind actually used, so a contract with no transient fields must not
    // contain a transient opcode at all.
    //
    // This asserts on the bytecode rather than the Yul because that is where
    // the claim lives: the emitted Yul is what we control, but only the
    // assembled bytecode proves nothing crept in through a helper.
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(
        "contract S:\n    u256 t\n\n    export fn a():\n        S.t = 1\n\n    export fn b() -> u256:\n        return S.t\n",
        &["--bytecode", "--solc", &solc_arg],
    );
    assert!(ok, "expected success, got:\n{}", out);
    let hex = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit()))
        .expect("no bytecode");
    let code = hex_to_bytes(&hex[2..]);
    let ops = storage_opcodes(&code);
    assert!(ops.contains(&"SSTORE") && ops.contains(&"SLOAD"), "expected persistent access, got {:?}", ops);
    assert!(
        !ops.contains(&"TSTORE") && !ops.contains(&"TLOAD"),
        "a contract with no transient fields must contain no transient opcode, got {:?}",
        ops
    );
}

#[test]
fn a_transient_only_contract_never_touches_persistent_storage() {
    // The converse of the test above, and the direction that actually matters:
    // a transient field must never reach an SSTORE. Every layout kind is
    // covered (scalar, dynamic array, mapping, and storage string) because
    // each has its own helper family, and the guarantee is only as good as its
    // weakest one.
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(
        "contract T:\n    transient u256 a\n    transient [u256] xs\n    \
         transient HashMap(u256, u256) m\n    transient String s\n\n    \
         export fn go():\n        T.a = 1\n        T.xs.push(2)\n        \
         T.m[3] = 4\n        T.s = \"hello\"\n\n    \
         export fn read() -> u256:\n        return T.a + T.xs.length + T.m[3]\n",
        &["--bytecode", "--solc", &solc_arg],
    );
    assert!(ok, "expected success, got:\n{}", out);
    let hex = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit()))
        .expect("no bytecode");
    let code = hex_to_bytes(&hex[2..]);
    let ops = storage_opcodes(&code);
    // Without this the test would pass vacuously on a contract that somehow
    // emitted no storage access at all.
    assert!(
        ops.contains(&"TSTORE") && ops.contains(&"TLOAD"),
        "expected transient access, got {:?}",
        ops
    );
    assert!(
        !ops.contains(&"SSTORE") && !ops.contains(&"SLOAD"),
        "a contract whose every field is transient must contain no persistent \
         storage opcode, got {:?}",
        ops
    );
}

// super.foo() reaches the version foo overrode. Flattening used to discard
// the parent's body at the override, so there was nothing left to call.
#[test]
fn super_resolves_to_the_overridden_method() {
    let (ok, out) = run_gumc(
        "class Base:\n    u256 v\n\n    fn label() -> u256:\n        return 10\n\n\
         [Base]\nclass Child:\n    fn label() -> u256:\n        return super.label() + 5\n\n\
         contract C:\n    u256 z\n\n    export fn r():\n        C.z = 1\n",
    );
    assert!(ok, "expected success, got:\n{}", out);
    assert!(out.contains("function Child_super_label(self)"), "parent body not kept:\n{}", out);
    assert!(out.contains("Child_super_label(self)"), "super call not emitted:\n{}", out);
}

// super only means something inside an override.
#[test]
fn super_without_an_override_is_rejected() {
    let (ok, out) = run_gumc(
        "class Base:\n    u256 v\n\n    fn a() -> u256:\n        return 1\n\n\
         [Base]\nclass Child:\n    fn b() -> u256:\n        return super.a()\n\n\
         contract C:\n    u256 z\n\n    export fn r():\n        C.z = 1\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("has nothing to call"), "unexpected error:\n{}", out);
}

// No self, no parent to reach. A top-level fn has no receiver at all,
// unlike a contract entry point, whose self is the contract.
#[test]
fn super_outside_a_method_is_rejected() {
    let (ok, out) = run_gumc(
        "fn helper() -> u256:\n    return super.foo()\n\n\
         contract C:\n    u256 z\n\n    export fn r():\n        C.z = 1\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("only available inside a method"), "unexpected error:\n{}", out);
}

// A contract with two immutables and one ordinary storage field.
fn const_field_src() -> &'static str {
    "contract Cfg:\n    const Account owner\n    const u256 cap\n    u256 counter\n\n    \
     fn new(Account o, u256 c):\n        Cfg.owner = o\n        Cfg.cap = c\n\n    \
     export fn get_owner() -> Account:\n        return Cfg.owner\n\n    \
     export fn get_cap() -> u256:\n        return Cfg.cap\n\n    \
     export fn bump():\n        Cfg.counter = Cfg.counter + 1\n"
}

#[test]
fn a_const_field_is_read_from_code_and_never_from_storage() {
    // The whole point: an immutable read is a PUSH32 baked in at deploy, not a
    // 2100-gas SLOAD. Asserted on the bytecode, since only the assembled code
    // can show that no storage access survives.
    //
    // counter is a real storage field, so exactly one SLOAD and one SSTORE
    // are expected. Two immutables read via SLOAD would make it three.
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(const_field_src(), &["--bytecode", "--solc", &solc_arg]);
    assert!(ok, "expected success, got:\n{}", out);
    let hex = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit()))
        .expect("no bytecode");
    let ops = storage_opcodes(&hex_to_bytes(&hex[2..]));
    let sloads = ops.iter().filter(|o| **o == "SLOAD").count();
    assert_eq!(sloads, 1, "expected only counter's SLOAD, got {:?}", ops);
}

#[test]
fn a_const_field_from_a_constructor_arg_is_patched_at_deploy() {
    let (ok, out) = run_gumc(const_field_src());
    assert!(ok, "expected success, got:\n{}", out);
    // Written once, into the runtime code that is in memory but not yet
    // returned, the only moment it is writable.
    assert!(out.contains(r#"setimmutable(0, "Cfg_owner", _immv_owner)"#), "no setimmutable:\n{}", out);
    assert!(out.contains(r#"setimmutable(0, "Cfg_cap", _immv_cap)"#), "no setimmutable:\n{}", out);
    // Read back from the code everywhere else.
    assert!(out.contains(r#"loadimmutable("Cfg_owner")"#), "no loadimmutable:\n{}", out);
}

#[test]
fn a_const_field_occupies_no_storage_slot() {
    // Immutables live in code, so they must not reserve a slot, leaving them
    // in the layout would silently push every real field along by one.
    // counter is declared third and must still land on slot 0.
    let (ok, out) = run_gumc(const_field_src());
    assert!(ok, "expected success, got:\n{}", out);
    assert!(out.contains("sload(0x0)") || out.contains("sload(0)"), "counter should be slot 0:\n{}", out);
}

#[test]
fn a_const_field_is_absent_from_the_storage_lock() {
    // Same reason transients are: it owns no slot, so no upgrade can move or
    // orphan it, and committing one would only invite a spurious conflict.
    let dir = std::env::temp_dir().join(format!("gum_imm_lock_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let lock = dir.join("layout.json");
    let lock_arg = lock.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(const_field_src(), &["--lock", &lock_arg]);
    assert!(ok, "expected success, got:\n{}", out);
    let manifest = std::fs::read_to_string(&lock).expect("lockfile");
    assert!(manifest.contains("counter"), "the real storage field should be committed:\n{}", manifest);
    assert!(!manifest.contains("owner"), "immutable 'owner' must not be in the lock:\n{}", manifest);
    assert!(!manifest.contains("\"cap\""), "immutable 'cap' must not be in the lock:\n{}", manifest);
    let _ = std::fs::remove_dir_all(&dir);
}

// The assembled runtime hex for a source, or None when solc is absent.
fn bytecode_of(src: &str) -> Option<String> {
    let solc = find_solc()?;
    let solc_arg = solc.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(src, &["--bytecode", "--solc", &solc_arg]);
    assert!(ok, "expected success, got:\n{}", out);
    Some(
        out.lines()
            .map(str::trim)
            .find(|l| l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit()))
            .expect("no bytecode")
            .to_string(),
    )
}

#[test]
fn a_const_field_the_compiler_can_evaluate_costs_nothing() {
    // const states the intent, never changes after deploy, and lets the
    // compiler pick how to keep it. When it can work the value out itself
    // there is nothing to patch at deploy: no setimmutable, no constructor
    // argument, no creation-code machinery.
    //
    // Byte-identical output to writing the literal by hand is the only
    // assertion that actually proves that. Merely "smaller" would pass even if
    // some vestige of the deploy-time path survived.
    let folded = match bytecode_of(
        "contract A:\n    const u256 cap\n\n    fn new():\n        A.cap = 100\n\n    \
         export fn get() -> u256:\n        return A.cap\n",
    ) {
        Some(b) => b,
        None => {
            eprintln!("skipping: no solc");
            return;
        }
    };
    let literal = bytecode_of(
        "contract A:\n    u256 z\n\n    export fn get() -> u256:\n        return 100\n",
    )
    .expect("solc present");
    assert_eq!(folded, literal, "a const the compiler can evaluate must cost exactly nothing");
}

#[test]
fn a_const_field_the_compiler_cannot_evaluate_is_patched_at_deploy() {
    // The other side of the same decision. A constructor argument does not
    let (ok, out) = run_gumc(
        "contract A:\n    const u256 cap\n\n    fn new(u256 c):\n        A.cap = c\n\n    \
         export fn get() -> u256:\n        return A.cap\n",
    );
    assert!(ok, "expected success, got:\n{}", out);
    assert!(out.contains(r#"setimmutable(0, "A_cap", _immv_cap)"#), "expected a deploy-time patch:\n{}", out);
    assert!(out.contains(r#"loadimmutable("A_cap")"#), "expected a code read:\n{}", out);
}

#[test]
fn a_const_field_is_only_folded_when_it_is_unconditional() {
    // The fold must not fire on a value only one branch produces, nor when a
    // later write would make the folded literal a lie. Both keep the
    // deploy-time patch, which is always correct.
    for (label, body) in [
        ("assigned in a branch", "        if c:\n            A.cap = 100\n        else:\n            A.cap = 200"),
        ("assigned twice", "        A.cap = 100\n        A.cap = 200"),
    ] {
        let src = format!(
            "contract A:\n    const u256 cap\n\n    fn new(bool c):\n{}\n\n    \
             export fn get() -> u256:\n        return A.cap\n",
            body
        );
        let (ok, out) = run_gumc(&src);
        assert!(ok, "{label}: expected success, got:\n{}", out);
        assert!(
            out.contains(r#"loadimmutable("A_cap")"#),
            "{label}: must not fold, expected the deploy-time path:\n{}",
            out
        );
    }
}

#[test]
fn a_const_field_is_not_folded_when_the_literal_would_need_masking() {
    // 300 does not fit a u8. A stored field would have been masked to 44 on the
    // way in, so folding the literal verbatim would make the contract report a
    // value it could never actually hold.
    let (ok, out) = run_gumc(
        "contract A:\n    const u8 small\n\n    fn new():\n        A.small = 300\n\n    \
         export fn get() -> u8:\n        return A.small\n",
    );
    assert!(ok, "expected success, got:\n{}", out);
    assert!(
        out.contains(r#"loadimmutable("A_small")"#),
        "an out-of-range literal must not be folded verbatim:\n{}",
        out
    );
}

#[test]
fn a_const_field_without_a_constructor_is_rejected() {
    // Nothing could ever set it: it would read zero for the life of the
    // contract. That is exactly the proxy-pattern mistake, so the message
    // points at the alternative.
    let (ok, out) = run_gumc(
        "contract C:\n    const u256 a\n\n    export fn g() -> u256:\n        return C.a\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("has no fn new()"), "unexpected error:\n{}", out);
}

#[test]
fn a_const_field_the_constructor_never_assigns_is_rejected() {
    let (ok, out) = run_gumc(
        "contract C:\n    const u256 a\n    const u256 b\n\n    fn new(u256 x):\n        \
         C.a = x\n\n    export fn g() -> u256:\n        return C.b\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("never assigned"), "unexpected error:\n{}", out);
}

// A constructor body wrapped in a contract that declares one immutable.
fn ctor_body(body: &str) -> String {
    format!(
        "enum Error:\n    Bad()\n\ncontract C:\n    const u256 a\n\n    fn new(u256 x, bool c):\n{}\n\n    \
         export fn g() -> u256:\n        return C.a\n",
        body
    )
}

#[test]
fn a_const_field_assigned_on_only_some_paths_is_rejected() {
    // The gap that matters: on the paths that skip the assignment the field is
    // baked into the deployed code as zero, permanently and silently. "Is it
    // assigned somewhere" is not a strong enough question, it must be assigned
    // on every path that reaches the end of the constructor.
    for (label, body) in [
        ("if with no else", "        if c:\n            C.a = x"),
        // A loop may execute zero times, so an assignment inside one
        // guarantees nothing at all.
        ("only inside a while", "        while c:\n            C.a = x"),
        ("only inside a for", "        for i in [1, 2]:\n            C.a = i"),
    ] {
        let (ok, out) = run_gumc(&ctor_body(body));
        assert!(!ok, "{label}: expected a compile error, got success:\n{}", out);
        assert!(
            out.contains("not on every path"),
            "{label}: expected a path-coverage error, got:\n{}",
            out
        );
    }
}

#[test]
fn a_const_field_assigned_on_every_path_is_accepted() {
    // The other half: the analysis must not reject correct code. A branch
    // either assigns or diverges, a branch that reverts never reaches the
    // deployed contract, so it owes nothing.
    for (label, body) in [
        ("unconditional", "        C.a = x"),
        ("both branches assign", "        if c:\n            C.a = x\n        else:\n            C.a = 1"),
        ("else diverges", "        if c:\n            C.a = x\n        else:\n            revert Error.Bad()"),
        ("if diverges", "        if c:\n            revert Error.Bad()\n        else:\n            C.a = x"),
        // Covered unconditionally first, so a later conditional write is just
        // a reassignment and cannot un-assign it.
        ("assigned then re-assigned", "        C.a = 1\n        if c:\n            C.a = x"),
    ] {
        let (ok, out) = run_gumc(&ctor_body(body));
        assert!(ok, "{label}: expected success, got:\n{}", out);
    }
}

#[test]
fn reading_a_const_field_inside_the_constructor_is_rejected() {
    // Its value is written into the code only once the constructor returns, so
    let (ok, out) = run_gumc(&ctor_body("        C.a = x\n        var y = C.a + 1"));
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("reads const field 'a'"), "unexpected error:\n{}", out);
}

#[test]
fn assigning_a_const_field_outside_the_constructor_is_rejected() {
    let (ok, out) = run_gumc(
        "contract C:\n    const u256 a\n\n    fn new(u256 x):\n        C.a = x\n\n    \
         export fn s(u256 y):\n        C.a = y\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("cannot be written afterwards"), "unexpected error:\n{}", out);
}

#[test]
fn a_const_field_on_a_plain_class_is_rejected() {
    let (ok, out) = run_gumc(
        "class P:\n    const u256 a\n\ncontract C:\n    u256 z\n\n    export fn g():\n        C.z = 1\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("cannot be const"), "unexpected error:\n{}", out);
}

#[test]
fn a_field_cannot_be_both_transient_and_const() {
    // Accepted by the grammar precisely so this explanation can be given.
    let (ok, out) = run_gumc(
        "contract C:\n    transient const u256 a\n\n    fn new(u256 x):\n        C.a = x\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("cannot be both transient and const"), "unexpected error:\n{}", out);
}

// Pulls the ABI JSON out of gumc's stdout.
fn abi_of(out: &str) -> serde_json::Value {
    let start = out.find("ABI JSON Generated:").expect("no ABI in output");
    let open = out[start..].find('[').expect("no ABI array") + start;
    // The ABI is pretty-printed, so its closing bracket is the first ] that
    // starts a line.
    let close = out[open..].find("\n]").expect("unterminated ABI array") + open + 2;
    serde_json::from_str(&out[open..close]).expect("ABI is not valid JSON")
}

fn keccak_hex(s: &str) -> String {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(s.as_bytes());
    k.finalize(&mut out);
    format!("0x{}", out.iter().map(|b| format!("{:02x}", b)).collect::<String>())
}

// The signature an event's published ABI entry describes.
fn abi_event_signature(e: &serde_json::Value) -> String {
    let args: Vec<String> = e["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap().to_string())
        .collect();
    format!("{}({})", e["name"].as_str().unwrap(), args.join(","))
}

#[test]
fn event_abi_reconciles_with_the_topic0_actually_emitted() {
    // The point of the event ABI is that an indexer can decode the logs this
    // contract really produces. That holds only if keccak256 of the signature
    // the ABI describes is the topic0 in the bytecode. Anything less, an ABI
    // that merely looks well-formed, silently mis-decodes every log.
    //
    // Checked against token.gum rather than a snippet so it covers the shape
    // real code uses.
    let src = std::fs::read_to_string(repo_root().join("examples/token.gum")).expect("token.gum");
    let (ok, out) = run_gumc(&src);
    assert!(ok, "expected success, got:\n{}", out);

    let abi = abi_of(&out);
    let events: Vec<&serde_json::Value> =
        abi.as_array().unwrap().iter().filter(|e| e["type"] == "event").collect();
    assert_eq!(events.len(), 2, "token.gum logs two events, got {:?}", events);

    for e in events {
        let topic0 = keccak_hex(&abi_event_signature(e));
        assert!(
            out.contains(&topic0),
            "event '{}' publishes signature '{}' -> {}, which appears nowhere in the emitted Yul. \
             The ABI and the bytecode disagree.",
            e["name"],
            abi_event_signature(e),
            topic0
        );
    }
}

#[test]
fn token_transfer_event_is_the_canonical_erc20_one() {
    // Pins the reconciliation above to an external fact: this is the real
    // ERC20 Transfer topic0, the one every wallet and explorer already knows.
    // If gum emits it, Etherscan decodes token.gum's transfers with no help.
    const ERC20_TRANSFER: &str =
        "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    assert_eq!(keccak_hex("Transfer(address,address,uint256)"), ERC20_TRANSFER);

    let src = std::fs::read_to_string(repo_root().join("examples/token.gum")).expect("token.gum");
    let (ok, out) = run_gumc(&src);
    assert!(ok, "expected success, got:\n{}", out);

    let abi = abi_of(&out);
    let transfer = abi
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "event" && e["name"] == "Transfer")
        .expect("no Transfer event in the ABI");
    assert_eq!(abi_event_signature(transfer), "Transfer(address,address,uint256)");
    assert!(out.contains(ERC20_TRANSFER), "canonical ERC20 Transfer topic0 not in the Yul");
}

#[test]
fn event_abi_marks_indexed_fields_and_names_them_from_the_call_site() {
    // An event's fields have no declaration, enum TokenLogs: Transfer names
    // a variant and nothing more. Names therefore come from the log() argument
    // when it is a bare identifier, and indexed-ness from the indexed() wrapper.
    let src = std::fs::read_to_string(repo_root().join("examples/token.gum")).expect("token.gum");
    let (ok, out) = run_gumc(&src);
    assert!(ok, "expected success, got:\n{}", out);

    let abi = abi_of(&out);
    let transfer = abi
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "event" && e["name"] == "Transfer")
        .expect("no Transfer event");
    let fields: Vec<(&str, &str, bool)> = transfer["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["name"].as_str().unwrap(),
                i["type"].as_str().unwrap(),
                i["indexed"].as_bool().expect("event input must carry indexed"),
            )
        })
        .collect();
    assert_eq!(
        fields,
        vec![("sender", "address", true), ("to", "address", true), ("amount", "uint256", false)]
    );
}

#[test]
fn an_event_entry_carries_no_outputs_or_state_mutability() {
    // Those keys belong to functions. An event entry carrying them is not a
    // shape the ABI spec allows, and strict decoders reject it.
    let src = std::fs::read_to_string(repo_root().join("examples/token.gum")).expect("token.gum");
    let (ok, out) = run_gumc(&src);
    assert!(ok, "expected success, got:\n{}", out);

    for e in abi_of(&out).as_array().unwrap() {
        if e["type"] != "event" {
            continue;
        }
        assert!(e.get("outputs").is_none(), "event has outputs: {}", e);
        assert!(e.get("stateMutability").is_none(), "event has stateMutability: {}", e);
        assert_eq!(e["anonymous"], serde_json::json!(false));
    }
    // ...and the converse: a function must not sprout an indexed key.
    let f = abi_of(&out)
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "function")
        .cloned()
        .expect("no function entry");
    for i in f["inputs"].as_array().unwrap() {
        assert!(i.get("indexed").is_none(), "function input has indexed: {}", i);
    }
}

#[test]
fn logging_one_event_with_two_different_shapes_is_rejected() {
    // An event name maps to exactly one ABI entry. Two sites disagreeing on
    // field types also means two different topic0s under one name, which no
    // ABI can express, so this must be an error, not a coin flip over which
    // shape gets published.
    let (ok, out) = run_gumc(
        "enum L:\n    E\n\ncontract T:\n    export fn a(u256 x):\n        log(L.E, x)\n\n    \
         export fn b(Account y):\n        log(L.E, y)\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("two different shapes"),
        "expected a shape-conflict error, got:\n{}",
        out
    );
}

#[test]
fn indexed_disagreement_across_log_sites_is_rejected() {
    // Same types, but different topics: the signature (and so topic0) matches,
    // yet the two logs put the field in different places. One published ABI
    // cannot describe both, and a decoder would read the wrong field.
    let (ok, out) = run_gumc(
        "enum L:\n    E\n\ncontract T:\n    export fn a(Account x):\n        log(L.E, indexed(x))\n\n    \
         export fn b(Account y):\n        log(L.E, y)\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("two different shapes"), "expected a shape conflict, got:\n{}", out);
}

#[test]
fn a_contract_that_logs_nothing_has_no_event_entries() {
    let (ok, out) = run_gumc("contract S:\n    u256 t\n\n    export fn a():\n        S.t = 1\n");
    assert!(ok, "expected success, got:\n{}", out);
    assert!(
        !abi_of(&out).as_array().unwrap().iter().any(|e| e["type"] == "event"),
        "unexpected event entry:\n{}",
        out
    );
}

// Which storage opcodes a bytecode actually contains. PUSH immediates are
// skipped so their data bytes aren't misread as opcodes.
fn storage_opcodes(code: &[u8]) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let op = code[i];
        let name = match op {
            0x54 => Some("SLOAD"),
            0x55 => Some("SSTORE"),
            0x5c => Some("TLOAD"),
            0x5d => Some("TSTORE"),
            _ => None,
        };
        if let Some(n) = name {
            if !out.contains(&n) {
                out.push(n);
            }
        }
        if (0x60..=0x7f).contains(&op) {
            i += (op - 0x5f) as usize;
        }
        i += 1;
    }
    out
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("bad hex"))
        .collect()
}

#[test]
fn if_condition_is_type_checked() {
    // Regression: the condition used to be discarded, so neither of these was
    // caught.
    assert_compile_fails("contract App:\n    export fn f(u256 x):\n        if x:\n            return\n");
    assert_compile_fails("contract App:\n    export fn f():\n        if not_a_thing:\n            return\n");
}

#[test]
fn eip7702_delegation_decode_has_the_right_shape() {
    // Pins the emitted Yul against the EIP-7702 spec: a delegated account's
    // code is exactly 23 bytes of 0xef0100 ++ <20-byte target>. The behavior
    // itself is execution-verified against a real revm 7702 account -- see
    let src = "use gum.defaults.Account\n\ncontract App:\n    export fn d(Account a) -> Account:\n        return a.delegated_to()\n";
    assert_output_contains(src, "eq(extcodesize(a), 23)");     // indicator length
    assert_output_contains(src, "extcodecopy(a, p, 0, 23)");
    assert_output_contains(src, "eq(shr(232, mload(p)), 0xef0100)"); // top 3 bytes = marker
    // target = bytes 3..23 -> shift the 3 marker bytes off the top, keep 160 bits
    assert_output_contains(src, "and(shr(72, mload(p)), 0xffffffffffffffffffffffffffffffffffffffff)");
}

#[test]
fn p256_verify_calls_the_precompile_at_0x100() {
    // EIP-7951 (L1) / RIP-7212 (L2s): precompile 0x100, 160-byte input
    // (h, r, s, qx, qy), 32-byte output.
    let src = "use gum.defaults.crypto\n\ncontract App:\n    export fn v(u256 h, u256 r, u256 s, u256 qx, u256 qy) -> bool:\n        return Crypto.verify_p256(h, r, s, qx, qy)\n";
    assert_output_contains(src, "staticcall(gas(), 0x100, p, 160, add(p, 160), 32)");
    // An invalid signature returns EMPTY data while the staticcall still
    // reports success, without this check, stale memory reads as a verdict.
    assert_output_contains(src, "eq(returndatasize(), 32)");
}

#[test]
fn p256_verify_arity_is_checked() {
    // Must not silently fall through to the class's declared stub (returns 0).
    assert_compile_fails(
        "use gum.defaults.crypto\n\ncontract App:\n    export fn v(u256 h) -> bool:\n        return Crypto.verify_p256(h)\n",
    );
}

// --- delete ---

#[test]
fn delete_rejects_a_whole_hashmap() {
    // A mapping has no enumerable key set, so delete m cannot clear anything.
    // Solidity accepts it and silently does nothing; gum refuses it outright.
    assert_compile_fails(
        "use gum.defaults.Account\n\ncontract D:\n    HashMap(Account, u256) bal\n\n    export fn bad():\n        delete D.bal\n",
    );
}

#[test]
fn delete_rejects_an_immutable_local() {
    assert_compile_fails("contract App:\n    export fn bad():\n        var x = 5\n        delete x\n");
}

#[test]
fn delete_rejects_a_computed_expression() {
    assert_compile_fails("contract App:\n    export fn bad(u256 a, u256 b):\n        delete a + b\n");
}

#[test]
fn delete_on_a_packed_field_preserves_its_slot_mates() {
    // Read-modify-write, not a blanket sstore of 0, deleting one packed field
    // must not zero the neighbours sharing its slot.
    let src = "contract D:\n    u8 a\n    u8 b\n\n    export fn wipe():\n        delete D.a\n";
    assert_output_contains(src, "sload(0)");
}

#[test]
fn delete_on_a_dynamic_array_clears_elements_and_length() {
    let src = "contract D:\n    [u256] xs\n\n    export fn wipe():\n        delete D.xs\n";
    assert_output_contains(src, "dpk_clear(0,");
    assert_output_contains(src, "sstore(len_slot, 0)");
}

#[test]
fn delete_on_a_storage_string_releases_its_data_slots() {
    let src = "contract D:\n    String name\n\n    export fn wipe():\n        delete D.name\n";
    assert_output_contains(src, "gum_sstr_clear(0)");
}

// --- Inheritance ---

#[test]
fn inherited_fields_come_before_the_child_s_own() {
    // Ancestors first, so adding a field to a child can never move an
    // inherited one to a different slot.
    let src = "class Base:\n    u256 a\n    u256 b\n\n[Base]\ncontract C:\n    u256 d\n\n    export fn get() -> u256:\n        return C.d\n";
    // a->0, b->1, d->2
    assert_output_contains(src, "sload(2)");
}

#[test]
fn a_child_inherits_its_parent_s_methods() {
    let src = "class Base:\n    u256 a\n\n    fn twice() -> u256:\n        return self.a * 2\n\n[Base]\ncontract C:\n    u256 z\n\n    export fn go() -> u256:\n        return C.twice()\n";
    assert_output_contains(src, "function C_twice()");
}

#[test]
fn a_child_method_overrides_its_parent_s() {
    let src = "class Base:\n    fn label() -> u256:\n        return 1\n\n[Base]\nclass Mid:\n    fn label() -> u256:\n        return 2\n\n[Mid]\ncontract C:\n    u256 z\n\n    export fn go() -> u256:\n        return C.label()\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    // The override, not Base's body.
    assert!(
        output.contains("function C_label() -> ret {\n          ret := 2"),
        "expected C.label() to be Mid's override returning 2, got:\n{}",
        output
    );
}

#[test]
fn inheritance_is_transitive() {
    let src = "class Base:\n    u256 a\n\n[Base]\nclass Mid:\n    u256 b\n\n[Mid]\ncontract C:\n    u256 c\n\n    export fn go() -> u256:\n        return C.a + C.b + C.c\n";
    assert_compiles(src);
}

#[test]
fn a_contract_inherits_its_parent_s_constructor() {
    // new is inherited like any other method, so a contract whose own
    // declaration has no new still gets a deploy-time constructor.
    let src = "class Base:\n    u256 a\n\n    fn new(u256 v):\n        self.a = v\n\n[Base]\ncontract C:\n    u256 z\n";
    assert_output_contains(src, "// --- Deployment Code ---");
    assert_output_contains(src, "C_new");
}

#[test]
fn inheriting_from_an_unknown_class_fails() {
    assert_compile_fails("[Nope]\nclass A:\n    u256 x\n");
}

#[test]
fn an_inheritance_cycle_fails() {
    assert_compile_fails("[B]\nclass A:\n    u256 x\n\n[A]\nclass B:\n    u256 y\n");
}

#[test]
fn re_declaring_an_inherited_field_fails() {
    // Shadowing a field would silently give it a second slot.
    assert_compile_fails("class A:\n    u256 x\n\n[A]\nclass B:\n    u256 x\n");
}

#[test]
fn an_ambiguous_inherited_method_fails() {
    assert_compile_fails(
        "class A:\n    fn f() -> u256:\n        return 1\n\nclass B:\n    fn f() -> u256:\n        return 2\n\n[A, B]\nclass C:\n    u256 z\n",
    );
}

#[test]
fn inheriting_from_a_contract_fails() {
    assert_compile_fails("contract K:\n    u256 x\n\n[K]\nclass B:\n    u256 y\n");
}

#[test]
fn an_interface_parent_requires_every_method() {
    // class C [ISomething] means "implements".
    assert_compile_fails("interface IThing:\n    fn ping(u256 x) -> bool\n\n[IThing]\nclass Impl:\n    u256 v\n");
}

#[test]
fn an_interface_parent_requires_matching_signatures() {
    assert_compile_fails(
        "interface IThing:\n    fn ping(u256 x) -> bool\n\n[IThing]\nclass Impl:\n    u256 v\n    fn ping(u256 x) -> u256:\n        return 1\n",
    );
}

#[test]
fn a_conforming_class_satisfies_its_interface_parent() {
    assert_compiles(
        "interface IThing:\n    fn ping(u256 x) -> bool\n\n[IThing]\nclass Impl:\n    u256 v\n    fn ping(u256 x) -> bool:\n        return true\n",
    );
}

#[test]
fn a_marker_parent_propagates_through_a_chain() {
    // B is [A], A is [Serializable]; B must still be serializable.
    let src = "use gum.defaults.Serializable\n\n[Serializable]\nclass A:\n    u256 x\n\n    fn new(u256 v):\n        self.x = v\n\n[A]\nclass B:\n    u256 y\n\ncontract App:\n    export fn go() -> [u8]:\n        var b = new B(7)\n        return b.serialize()\n";
    assert_output_contains(src, "function B_serialize");
}

#[test]
fn imports_are_transitive() {
    // An imported module's own use lines must be followed. std depends on
    // this: account.gum is class Account [Serializable] and imports
    // Serializable itself, so importing Account without following that import
    assert_compiles("use gum.defaults.Account\n\ncontract App:\n    export fn go(Account a) -> u256:\n        return a.balance()\n");
}

#[test]
fn the_standard_library_needs_no_search_path() {
    // Every test here compiles out of the system temp directory, which has no std/ in it or above it, so this passing at all is the property. Asserted explicitly so the reason does not get lost.
    let src = "use gum.defaults.String\n\ncontract C:\n    export fn f(String s) -> u256:\n        return s.length\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success with no base dir, got:\n{}", output);
    assert!(output.contains("Loading gum.defaults"), "String should resolve from the embedded table, got:\n{}", output);
}

#[test]
fn an_unresolvable_import_is_an_error() {
    // Skipping a module that does not resolve reported nothing at the import and surfaced later as a missing type, which is a long way to walk back from a typo.
    let (ok, output) = run_gumc("use gum.defaults.Nonsense\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n");
    assert!(!ok, "an unknown std module must fail, got:\n{}", output);
    assert!(output.contains("has no 'Nonsense'"), "expected a module error naming the symbol, got:\n{}", output);
    let (ok3, out3) = run_gumc("use gum.nope.Thing\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n");
    assert!(!ok3, "an unknown std module must fail, got:\n{}", out3);
    assert!(out3.contains("does not name anything in the standard library"), "expected a module error, got:\n{}", out3);

    let (ok2, out2) = run_gumc("use not_here\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n");
    assert!(!ok2, "a missing local module must fail, got:\n{}", out2);
    assert!(out2.contains("cannot read module"), "expected a read error, got:\n{}", out2);
}

#[test]
fn a_module_imported_twice_is_only_loaded_once() {
    // Both of these pull in Serializable transitively; loading it twice would
    // duplicate every declaration it contains.
    let src = "use gum.defaults.Account\nuse gum.defaults.Serializable\n\ncontract App:\n    export fn go(Account a) -> u256:\n        return a.balance()\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    assert_eq!(
        output.matches("Loading gum.defaults").count(),
        1,
        "Serializable should be loaded exactly once, got:\n{}",
        output
    );
}

// --- Parser error recovery ---

#[test]
fn every_broken_function_in_a_contract_is_reported_not_just_the_first() {
    // pest has no error recovery: the retrn on line 5 would abort the whole
    // parse and hide the broken expression on line 11 entirely.
    //
    let src = "contract C:\n    u256 x\n\n    export fn one() -> u256:\n        retrn C.x\n\n    export fn two() -> u256:\n        return C.x\n\n    export fn three(u256 a) -> u256:\n        return a +\n\n    export fn four() -> u256:\n        return 4\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(output.contains("2 syntax errors found"), "expected both errors, got:\n{}", output);
    // Reported against the real lines of the original file, not the chunk.
    assert!(output.contains("--> 5:"), "first error should be on line 5, got:\n{}", output);
    assert!(output.contains("--> 11:"), "second error should be on line 11, got:\n{}", output);
}

#[test]
fn every_broken_statement_in_one_function_is_reported_not_just_the_first() {
    // Recovery goes a level deeper than the member: two malformed statements in
    // the same function body are both reported. pest alone would stop at the
    // first (line 3) and never reach the second (line 5).
    let src = "contract C:\n    export fn f(u256 a) -> u256:\n        retrn a\n        mut u256 r = 0\n        r = r @@ 1\n        return r\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(output.contains("2 syntax errors found"), "expected both statement errors, got:\n{}", output);
    assert!(output.contains("--> 3:"), "first error should be on line 3, got:\n{}", output);
    assert!(output.contains("--> 5:"), "second error should be on line 5, got:\n{}", output);
}

#[test]
fn a_bad_statement_and_a_bad_signature_still_leave_valid_functions_alone() {
    // A function with a broken statement (line 3) sits beside a wholly valid
    // one; only the broken statement is reported, and the good function compiles.
    let src = "contract C:\n    export fn bad() -> u256:\n        return @\n\n    export fn good() -> u256:\n        return 1\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(output.contains("1 syntax error"), "expected exactly one error, got:\n{}", output);
    assert!(output.contains("--> 3:"), "error should be on line 3, got:\n{}", output);
}

#[test]
fn a_broken_declaration_does_not_hide_a_later_one_of_a_different_kind() {
    // A malformed fn and a malformed contract body: different declaration
    // rules, both reported.
    let src = "contract C:\n    u256 @@@\n\n    export fn bad() -> u256:\n        return @\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(
        output.contains("2 syntax errors found"),
        "a broken fn must not hide a broken contract, got:\n{}",
        output
    );
}

#[test]
fn an_indentation_error_still_stops_at_the_first_one() {
    // Recovery is per top-level declaration, and the split into declarations
    // is what the indent preprocessor produces, so a bad indent is reported
    // alone, before there is any structure to recover within. Documenting the
    let src = "contract C\n    u256 x\n\n    export fn a() -> u256:\n        return 1\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(output.contains("Indentation error"), "expected an indent error, got:\n{}", output);
}

#[test]
fn a_string_literal_containing_braces_does_not_split_a_declaration() {
    // The splitter tracks brace depth; a { or ; inside a string is text, not
    // structure. Getting this wrong would cut the function in half and report a
    // syntax error in code that is perfectly valid.
    assert_compiles("contract App:\n    export fn f() -> String:\n        return \"a { b } c ; d\"\n");
}

#[test]
fn a_comment_containing_braces_does_not_split_a_declaration() {
    assert_compiles("contract App:\n    export fn f() -> u256:\n        // } ; { not real structure\n        return 1\n");
}

#[test]
fn an_fstring_with_interpolation_does_not_split_a_declaration() {
    assert_compiles("contract App:\n    export fn f(u256 n) -> String:\n        return f\"n is {n}; ok\"\n");
}

#[test]
fn an_unsafe_block_s_nested_braces_do_not_split_a_declaration() {
    // Raw Yul carries its own nested blocks. The splitter must count them, or
    // it would end the function at the first inner }.
    let src = "contract App:\n    export fn f(u256 a) -> u256:\n        mut u256 r = 0\n        unsafe:\n            for { let i := 0 } lt(i, a) { i := add(i, 1) } {\n                r := add(r, i)\n            }\n        return r\n";
    assert_compiles(src);
}

#[test]
fn trailing_garbage_after_a_declaration_is_not_silently_dropped() {
    // A declaration rule will happily match a valid prefix; decl_unit's EOI
    // is what makes the leftovers an error instead of invisible.
    assert_compile_fails("use gum.defaults.Account extra\n");
}

#[test]
fn copying_a_whole_storage_array_of_scalars_is_allowed() {
    assert_compiles("contract C:\n    [u256] arr\n\n    export fn ok() -> u256:\n        var a = C.arr\n        return a[0]\n");
}

// A struct element is a group of slots with no memory form, so there is nothing to copy it into.
#[test]
fn copying_a_storage_array_of_structs_is_rejected() {
    let (ok, out) = run_gumc(
        "class S:\n    u256 a\n    u256 b\n\ncontract C:\n    [S] xs\n\n    export fn bad():\n        var a = C.xs\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(out.contains("no memory form"), "unexpected error:\n{}", out);
}

#[test]
fn using_a_storage_array_in_place_still_works() {
    // The rejection above must not catch the legitimate uses.
    assert_compiles(
        "contract C:\n    [u256] arr\n    u256 total\n\n    export fn ok() -> u256:\n        C.arr.push(1)\n        C.total = 0\n        for x in C.arr:\n            C.total = C.total + x\n        C.total = C.total + C.arr[0] + C.arr.length\n        C.arr.pop()\n        return C.total\n",
    );
}

// --- receive / fallback ---

#[test]
fn receive_is_dispatched_on_empty_calldata() {
    let src = "use gum.defaults.Message\n\ncontract V:\n    u256 got\n\n    export payable fn receive():\n        V.got = V.got + Message.value()\n";
    assert_output_contains(src, "if iszero(calldatasize()) {");
    assert_output_contains(src, "receive_impl()");
}

#[test]
fn fallback_catches_an_unmatched_selector() {
    let src = "contract V:\n    u256 got\n\n    export fn fallback():\n        V.got = 1\n\n    export fn poke():\n        V.got = 2\n";
    assert_output_contains(src, "default {");
    assert_output_contains(src, "fallback_impl()");
}

#[test]
fn receive_and_fallback_get_no_abi_selector() {
    // A receive() selector would dispatch nowhere; the ABI must describe them
    // by entry type instead.
    let src = "use gum.defaults.Message\n\ncontract V:\n    u256 got\n\n    export payable fn receive():\n        V.got = V.got + Message.value()\n\n    export fn fallback():\n        V.got = 0\n";
    assert_output_contains(src, "\"type\": \"receive\"");
    assert_output_contains(src, "\"type\": \"fallback\"");
    let (_, output) = run_gumc(src);
    assert!(
        !output.contains("/* receive */") && !output.contains("/* fallback */"),
        "receive/fallback must not appear as selector cases, got:\n{}",
        output
    );
}

#[test]
fn a_payable_receive_suppresses_the_hoisted_nonpayable_guard() {
    // The hoist is only sound when nothing is payable. A payable receive counts.
    let src = "use gum.defaults.Message\n\ncontract V:\n    u256 got\n\n    export payable fn receive():\n        V.got = V.got + Message.value()\n\n    export fn poke():\n        V.got = 0\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    // poke() carries its own guard... (matched by shape, not by a pinned
    // selector value)
    assert!(
        output.contains("/* poke */ {\n          if callvalue() { revert(0, 0) }"),
        "poke() must carry its own nonpayable guard, got:\n{}",
        output
    );
    // ...instead of one hoisted above the switch, which would reject the ETH
    // that receive() exists to accept.
    assert!(
        !output.contains("let selector := shr(224, calldataload(0))\n      if callvalue() { revert(0, 0) }"),
        "the hoisted guard must be suppressed when receive() is payable, got:\n{}",
        output
    );
}

#[test]
fn receive_must_be_payable() {
    assert_compile_fails("contract App:\n    export fn receive():\n        var x = 1\n");
}

#[test]
fn receive_takes_no_parameters() {
    assert_compile_fails("contract App:\n    export payable fn receive(u256 x):\n        var y = x\n");
}

#[test]
fn receive_returns_nothing() {
    assert_compile_fails("contract App:\n    export payable fn receive() -> u256:\n        return 1\n");
}

#[test]
fn receive_must_be_exported() {
    // Inside a contract, receive is the reserved bare-ETH entry point, so a
    // non-exported one would silently never be reachable.
    assert_compile_fails("contract C:\n    u256 x\n\n    payable fn receive():\n        C.x = 1\n");
}

#[test]
fn receive_is_only_reserved_inside_a_contract() {
    // A top-level fn receive() is an ordinary internal helper, entry points
    // only exist inside a contract, so there is nothing to reserve out here.
    assert_compiles("fn receive() -> u256:\n    return 1\n\ncontract C:\n    export fn f() -> u256:\n        return receive()\n");
}

#[test]
fn fallback_may_be_nonpayable() {
    assert_compiles("contract V:\n    u256 got\n\n    export fn fallback():\n        V.got = 1\n");
}

// --- new ContractName(...) ---

#[test]
fn new_on_a_contract_emits_create_not_an_allocation() {
    // new on a plain class allocates memory; on a contract it must deploy.
    let src = "contract Child:\n    u256 v\n\n    fn new(u256 x):\n        self.v = x\n\ncontract Factory:\n    u256 n\n\n    export fn make(u256 x) -> Account:\n        return new Child(x)\n";
    assert_output_contains(src, "function __deploy_Child(a0) -> addr {");
    assert_output_contains(src, "datacopy(ptr, dataoffset(\"Child\"), size)");
    // One static arg: a single head word, straight into the blob.
    assert_output_contains(src, "let alen := 32");
    assert_output_contains(src, "let blob := add(ptr, size)");
    assert_output_contains(src, "mstore(add(blob, 0), a0)");
    assert_output_contains(src, "addr := create(0, ptr, add(size, alen))");
    // CREATE reports failure as address 0, not a revert.
    assert_output_contains(src, "if iszero(addr) { gum_bubble_revert() }");
}

#[test]
fn a_deployed_child_is_nested_inside_its_deployer_s_runtime() {
    // Child's object must appear inside Factory_runtime: only
    let src = "contract Child:\n    u256 v\n\ncontract Factory:\n    u256 n\n\n    export fn make() -> Account:\n        return new Child()\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    let factory = output.split("object \"Factory\" {").nth(1).expect("no Factory object");
    let runtime = factory
        .split("object \"Factory_runtime\" {")
        .nth(1)
        .expect("no Factory_runtime object");
    assert!(
        runtime.contains("object \"Child\" {"),
        "Child must be nested inside Factory_runtime, or it won't exist in the deployed code:\n{}",
        factory
    );
}

#[test]
fn a_contract_object_excludes_its_siblings_methods() {
    // Another contract's methods were never callable from here, and a sibling
    let src = "contract Child:\n    u256 v\n\ncontract Factory:\n    u256 n\n\n    export fn make() -> Account:\n        return new Child()\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    let child_obj = output
        .split("--- Contract: Child ---")
        .nth(1)
        .and_then(|s| s.split("--- Contract: Factory ---").next())
        .expect("no Child section");
    assert!(
        !child_obj.contains("Factory_make"),
        "Child's object must not carry Factory's methods, got:\n{}",
        child_obj
    );
}

#[test]
fn each_contract_gets_its_own_storage_slots_from_zero() {
    // Two contracts in a file are two deployments. Stacking the second after
    let src = "contract A:\n    u256 x\n    u256 y\n\n    export fn set():\n        A.x = 1\n        A.y = 2\n\ncontract B:\n    u256 p\n\n    export fn set():\n        B.p = 3\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    let b = output.split("--- Contract: B ---").nth(1).expect("no B section");
    assert!(b.contains("sstore(0, 3)"), "B.p must be slot 0, not stacked after A's fields:\n{}", b);
}

#[test]
fn deploying_a_contract_with_a_string_constructor_arg_encodes_head_and_tail() {
    // The child's constructor decoder reads a dynamic arg's head slot as an
    let src = "contract Child:\n    String name\n    u256 n\n\n    fn new(String s, u256 v):\n        self.name = s\n        self.n = v\n\ncontract Factory:\n    u256 c\n\n    export fn make(String s, u256 v) -> Account:\n        return new Child(s, v)\n";
    // head: 2 words. a0 is dynamic -> its head slot holds the tail offset.
    assert_output_contains(src, "let tail := 64");
    assert_output_contains(src, "mstore(add(blob, 0), tail)");
    assert_output_contains(src, "mstore(add(blob, tail), a0_len)");
    // the static arg goes straight into its head slot
    assert_output_contains(src, "mstore(add(blob, 32), a1)");
    // total length is computed at runtime, since a0's length is
    assert_output_contains(src, "alen := add(alen, add(32, a0_pad))");
    assert_assembles(src);
}

#[test]
fn deploying_a_contract_with_an_array_constructor_arg_encodes_it() {
    // The array is re-expanded to one ABI word per element after the creation
    // code, and the child decodes it back out of the codecopy'd blob.
    let src = "contract Child:\n    u256 n\n\n    fn new([u256] xs):\n        self.n = xs.length\n\ncontract Factory:\n    u256 c\n\n    export fn make([u256] xs) -> Account:\n        return new Child(xs)\n";
    assert_output_contains(src, "let a0_abi := gum_abi_arr_u256_size(a0)");
    assert_output_contains(src, "tail := add(tail, gum_abi_arr_u256_put(add(blob, tail), a0))");
    assert_output_contains(src, "gum_abi_arr_u256_mem(args_mem,");
    assert_assembles(src);
}

#[test]
fn deploying_a_contract_with_a_fixed_array_constructor_arg_encodes_it_inline() {
    // [T; N] is a static type on the wire: N words inline in the head, with
    // no offset and no tail.
    let src = "contract Child:\n    u256 n\n\n    fn new([u8; 3] xs, u256 v):\n        self.n = v\n\ncontract Factory:\n    u256 c\n\n    export fn make([u8; 3] xs, u256 v) -> Account:\n        return new Child(xs, v)\n";
    assert_output_contains(src, "pop(gum_abi_farr3_u8_put(add(blob, 0), a0))");
    assert_output_contains(src, "gum_abi_farr_put(dst, ptr, 3, 1)");
    // The array takes three head words, so the next argument starts at 96 ,
    assert_output_contains(src, "mstore(add(blob, 96), a1)");
    assert_output_contains(src, "let param_xs := gum_abi_farr3_u8_mem(args_mem, 0, _args_len)");
    assert_assembles(src);
}

#[test]
fn a_deployment_cycle_is_a_compile_error() {
    assert_compile_fails(
        "contract A:\n    u256 x\n\n    export fn make() -> Account:\n        return new B()\n\ncontract B:\n    u256 y\n\n    export fn make() -> Account:\n        return new A()\n",
    );
}

#[test]
fn new_on_a_plain_class_still_allocates_memory() {
    // The contract case must not have swallowed the memory-struct case.
    let src = "class Point:\n    u256 x\n\n    fn new(u256 v):\n        self.x = v\n\ncontract C:\n    export fn go() -> u256:\n        var p = new Point(7)\n        return p.x\n";
    assert_output_contains(src, "allocate_memory");
    let (_, output) = run_gumc(src);
    assert!(!output.contains("__deploy_Point"), "a plain class must not be deployed");
}

// --- Array ABI ---

#[test]
fn an_array_argument_decodes_from_its_abi_offset_not_as_a_scalar() {
    // The head word of a T[] is an offset, not the value. Decoding it as a
    // scalar handed the body a calldata offset and called it an array pointer ,
    // while the published ABI said uint256[], so callers encoded it properly.
    let src = "contract C:\n    export fn sum([u256] xs) -> u256:\n        mut u256 s = 0\n        for x in xs:\n            s = s + x\n        return s\n";
    assert_output_contains(src, "gum_abi_arr_u256_cd(add(4, calldataload(4)))");
    assert_output_contains(src, "ptr := gum_abi_arr_cd(off, 32)");
    assert_output_contains(src, "\"type\": \"uint256[]\"");
    assert_assembles(src);
}

#[test]
fn a_narrow_array_converts_between_wire_and_memory_widths() {
    // The ABI gives a uint8 a whole 32-byte word; memory packs it to one byte.
    // A flat copy would be wrong in both directions.
    let src = "contract C:\n    export fn echo([u8] xs) -> [u8]:\n        return xs\n";
    assert_output_contains(src, "let param_xs := gum_abi_arr_u8_cd(add(4, calldataload(4)))");
    assert_output_contains(src, "ptr := gum_abi_arr_cd(off, 1)");
    assert_output_contains(src, "let _w := gum_abi_arr_u8_put(add(_out, 32), _p)");
    assert_output_contains(src, "written := gum_abi_arr_put(dst, ptr, 1)");
    assert_assembles(src);
}

#[test]
fn indexing_a_memory_array_is_bounds_checked() {
    // Storage arrays were always checked; memory ones were not, so xs[i] on a
    // [T] argument read whatever sat past the end instead of reverting.
    let src = "contract C:\n    export fn at([u256] xs, u256 i) -> u256:\n        return xs[i]\n";
    assert_output_contains(src, "function gum_marr_addr(ptr, i, esz) -> a {");
    assert_output_contains(src, "if iszero(lt(i, div(mload(ptr), esz)))");
    assert_assembles(src);
}

#[test]
fn writing_a_memory_array_element_is_bounds_checked_once() {
    // The address expression performs the check, so it must be bound to a local
    // rather than evaluated twice by the read-modify-write.
    let src = "contract C:\n    export fn set([u8] xs, u256 i, u8 v) -> u256:\n        xs[i] = v\n        return xs.length\n";
    assert_output_contains_numbered(src, "let __ma_", " := gum_marr_addr");
    assert_assembles(src);
}

#[test]
fn memory_array_length_is_an_element_count_not_a_byte_count() {
    // Word 0 of a memory array holds its length in bytes. .length must
    // divide by the stride, reading the word raw is right only for [u8].
    assert_output_contains(
        "contract C:\n    export fn n([u256] xs) -> u256:\n        return xs.length\n",
        "div(mload(xs), 32)",
    );
}

#[test]
fn an_array_of_arrays_crosses_the_abi() {
    // The element is dynamic, so it carries an offset rather than its bytes and the outer array needs its own codec rather than a stride.
    let src = "contract C:\n    export fn f([[u256]] xs) -> u256:\n        return xs[0][1]\n";
    assert_output_contains(src, "\"type\": \"uint256[][]\"");
    assert_output_contains(src, "let param_xs := gum_abi_arr_arr_u256_cd(add(4, calldataload(4)))");
    // The inner decode is reached through the offset table, not by striding over inline data.
    assert_output_contains(src, "mstore(add(add(ptr, 32), mul(i, 32)), gum_abi_arr_u256_cd(add(base, eo)))");
    assert_assembles(src);
}

#[test]
fn a_dynamic_value_inside_a_storage_aggregate_is_rejected() {
    // Each of these compiled, and each read a storage word and then used it as a memory address. [String] was worst: it laid a String out as an 8-byte packed scalar.
    assert_compile_fails("contract C:\n    [[u256]] g\n\n    export fn f(u256 i, u256 j) -> u256:\n        return C.g[i][j]\n");
    assert_compile_fails("contract C:\n    [[u256]; 2] g\n\n    export fn f() -> u256:\n        return 1\n");
    assert_compile_fails("use gum.defaults.String\n\ncontract C:\n    [String] g\n\n    export fn f() -> u256:\n        return 1\n");
    // A mapping to an array of a dynamic element still has no per-element layout.
    assert_compile_fails("use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, [[u256]]) m\n\n    export fn f() -> u256:\n        return 1\n");
    // Storage is only a contract's own fields, so the same type as a parameter is untouched.
    assert_compiles("contract C:\n    export fn f([[u256]] xs) -> u256:\n        return xs[0][0]\n");
    // And what has a real layout still has one.
    assert_compiles("use gum.defaults.String\n\ncontract C:\n    String s\n\n    export fn f() -> u256:\n        return C.s.length\n");
    assert_compiles("class P:\n    u256 x\n\ncontract C:\n    [P] xs\n\n    export fn f(u256 i) -> u256:\n        return C.xs[i].x\n");
    assert_compiles("use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, HashMap(Account, u256)) m\n\n    export fn f(Account a, Account b) -> u256:\n        return C.m[a][b]\n");
    assert_compiles("contract C:\n    [u256] xs\n\n    export fn f(u256 i) -> u256:\n        return C.xs[i]\n");
    // A String/Bytes mapping value gets its own slot region at keccak256(key ‖ p),
    // exactly like Solidity's mapping(K => string), so it does have a layout.
    assert_compiles("use gum.defaults.Account\nuse gum.defaults.String\n\ncontract C:\n    HashMap(Account, String) m\n\n    export fn f(Account a, String s):\n        C.m[a] = s\n\n    export fn g(Account a) -> String:\n        return C.m[a]\n");
    // A dynamic-array mapping value likewise has a layout (mapping(K => T[])): the
    // value slot holds the length, elements pack from keccak256(that slot).
    assert_compiles("use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, [u256]) m\n\n    export fn f(Account a, u256 v):\n        C.m[a].push(v)\n\n    export fn g(Account a, u256 i) -> u256:\n        return C.m[a][i]\n\n    export fn n(Account a) -> u256:\n        return C.m[a].length\n");
}

#[test]
fn a_string_array_across_the_abi_is_accepted() {
    // String has a dynamic-element codec now (gum_abi_str_cd/mem/put/size), so
    // string[] rides the wire like any other dynamic-element array, and nests.
    assert_compiles("use gum.defaults.String\n\ncontract C:\n    export fn f([String] xs) -> [String]:\n        return xs\n");
    assert_compiles("use gum.defaults.String\n\ncontract C:\n    export fn f([String] xs) -> String:\n        return xs[0]\n");
    assert_compiles("use gum.defaults.String\n\ncontract C:\n    export fn f([[String]] xs) -> u256:\n        return xs.length\n");
}

#[test]
fn a_dynamic_struct_crosses_the_abi_but_not_nested_or_in_an_array() {
    // A struct with a String (or array) field rides the wire as a dynamic tuple
    // now — head of (id, offset), then the name's tail — as an argument and a
    // return.
    assert_compiles("use gum.defaults.String\n\nclass Meta:\n    u256 id\n    String name\n\n    fn new(u256 i, String n):\n        self.id = i\n        self.name = n\n\ncontract C:\n    export fn echo(Meta m) -> Meta:\n        return m\n");
    assert_compiles("class Nums:\n    u256 id\n    [u256] xs\n\ncontract C:\n    export fn f(Nums n) -> u256:\n        return n.id\n");
    // But a dynamic struct as an *array element*, or a struct nested in a struct,
    // still has no codec.
    assert_compile_fails("use gum.defaults.String\n\nclass Meta:\n    u256 id\n    String name\n\ncontract C:\n    export fn f([Meta] ms) -> u256:\n        return 1\n");
}

#[test]
fn interface_args_are_encoded_head_tail() {
    // The arg blob is laid out from the declared types, so a string goes out as an offset and a tail rather than as its memory pointer in one word.
    let src = "contract C:\n    export fn f(Account t, String s) -> u256:\n        return ISink(t).take(s)\n\ninterface ISink:\n    fn take(String s) -> u256\n";
    assert_output_contains(src, "let a0_len := gum_str_len(a0)");
    assert_output_contains(src, "mstore(add(blob, tail), a0_len)");
}

#[test]
fn an_interface_returning_a_non_scalar_decodes_it() {
    // A string and an array come back through their own decoders, not as the offset word that would read as a plausible small number.
    let src = "use gum.defaults.String\n\ninterface I:\n    fn name() -> String\n\ncontract C:\n    export fn f(Account t) -> u256:\n        var s = I(t).name()\n        return s.length\n";
    assert_output_contains(src, "gum_abi_str_mem(rd, mload(rd), returndatasize())");
    let arr = "interface I:\n    fn xs() -> [u256]\n\ncontract C:\n    export fn f(Account t) -> u256:\n        var a = I(t).xs()\n        return a.length\n";
    assert_output_contains(arr, "gum_abi_arr_u256_mem(rd, mload(rd), returndatasize())");
}

#[test]
fn a_dynamic_array_of_structs_across_the_abi_is_accepted() {
    // A static tuple is inline on the wire, so [P] needs no per-element offset and does encode.
    assert_compiles("class P:\n    u128 a\n    u256 b\n\ncontract C:\n    export fn f([P] xs) -> [P]:\n        return xs\n");
}

#[test]
fn an_array_of_non_static_structs_across_the_abi_is_rejected() {
    // The element struct must itself be all-scalar; a String field makes it dynamic, and that has no codec.
    assert_compile_fails("class W:\n    u256 z\n    String s\n\ncontract C:\n    export fn f([W] xs) -> u256:\n        return 1\n");
}

#[test]
fn a_fixed_array_of_structs_rides_inline_in_the_head() {
    // Every element is static, so the whole thing is static: no offset word, no count word, and the head is as wide as the elements together.
    let src = "class P:\n    u256 x\n\ncontract C:\n    export fn f([P; 2] xs, u256 v) -> u256:\n        return xs[1].x + v\n";
    assert_output_contains(src, "\"type\": \"tuple[2]\"");
    assert_output_contains(src, "let param_xs := gum_abi_farr2_P_cd(4)");
    // Two one-field tuples inline take 64 bytes, so v is read at 68 rather than at 36.
    assert_output_contains(src, "let param_v := calldataload(68)");
    assert_assembles(src);
}

#[test]
fn a_revert_counts_as_diverging_for_the_return_check() {
    // A revert ends the frame, so the path never reaches a missing return. check_returns knew about return, if/else and match but not revert, so this shape was rejected.
    assert_compiles("enum Error:\n    Bad(u256 x)\n\ncontract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            return x\n        revert Error.Bad(x)\n");
    // One branch returning and the other reverting is a complete function.
    assert_compiles("enum Error:\n    B(u256 x)\n\ncontract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            return 1\n        else:\n            revert Error.B(x)\n");
    // And the check must not have got weaker: a real missing return is still an error.
    assert_compile_fails("contract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            return x\n");
    assert_compile_fails("enum Error:\n    B(u256 x)\n\ncontract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            revert Error.B(x)\n");
}

#[test]
fn a_payload_free_enum_is_one_byte_like_solidity() {
    // size_of said 64 for every enum, which burned two storage slots per field, displaced every later field, and made the mapping and log paths write the memory pointer instead of the value.
    let src = "enum S:\n    A\n    B\n\ncontract C:\n    S state\n\n    export fn set(S s):\n        C.state = s\n";
    // One packed byte in one slot, exactly as Solidity lays an enum out.
    assert_output_contains(src, "sstore(0, or(and(sload(0), not(shl(0, 0xff))), shl(0, and(s, 0xff))))");
}

#[test]
fn a_payload_enum_has_no_storage_layout() {
    // A payload-carrying enum is a [tag][payload] pair that exists only in memory. Anywhere a size is needed it must be rejected, or it is laid out as 64 opaque bytes and every read returns a stale address.
    let head = "enum R:\n    Ok(u256 x)\n    Err\n\n";
    for body in [
        "contract C:\n    R r\n\n    export fn f() -> u256:\n        return 1\n",
        "use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, R) m\n\n    export fn f() -> u256:\n        return 1\n",
        "contract C:\n    [R] xs\n\n    export fn f() -> u256:\n        return 1\n",
        "class S:\n    R r\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n",
    ] {
        let (ok, out) = run_gumc(&format!("{}{}", head, body));
        assert!(!ok, "a payload enum must not be given a layout:\n{}", out);
        assert!(out.contains("has no storage layout"), "expected a layout error, got:\n{}", out);
    }
}

#[test]
fn an_enum_param_decodes_from_one_word() {
    // A payload-free enum is a plain u8 tag: one word in, bounds-checked against the
    // variant count (like Solidity), masked, and used as a value throughout. No
    // pointer, no allocation.
    // It used to copy size_of(enum) = 64 bytes, which ate the next argument as a phantom payload and left every later one reading past calldata as zero.
    let src = "enum S:\n    A\n    B\n\ncontract C:\n    export fn f(S s, u256 x) -> u256:\n        return x\n";
    assert_output_contains(src, "let param_s_raw := calldataload(4)");
    // Two variants, so any tag >= 2 reverts before it can enter the contract.
    assert_output_contains(src, "if iszero(lt(param_s_raw, 2)) { revert(0, 0) }");
    assert_output_contains(src, "let param_s := and(param_s_raw, 0xff)");
    assert_output_contains(src, "let param_x := calldataload(36)");
}

#[test]
fn an_enum_with_a_payload_across_the_abi_is_rejected() {
    // uint8 carries the tag and nothing else, so a payload has nowhere to go.
    assert_compile_fails("enum E:\n    Bare\n    Carrying(u256)\n\ncontract C:\n    export fn f(E e) -> u256:\n        return 1\n");
    // A plain enum stays fine.
    assert_compiles("enum S:\n    A\n    B\n\ncontract C:\n    export fn f(S s) -> S:\n        return s\n");
}

#[test]
fn a_struct_across_the_abi_is_accepted() {
    // Declaration order is ABI order while memory order is widest-first, so this also pins that the two are allowed to disagree.
    assert_compiles("use gum.defaults.Account

class P:
    u128 a
    u256 b
    Account c

contract C:
    export fn f(P p) -> P:
        return p
");
}

#[test]
fn a_struct_nesting_another_struct_across_the_abi_is_rejected() {
    // A struct field that is itself a struct is multi-word and would need the
    // dynamic-tuple codec applied recursively, which is not built; it is refused
    // rather than mis-encoded. (A String/array field, by contrast, now works.)
    assert_compile_fails("class I:
    u256 x

class O:
    u256 y
    I n

contract C:
    export fn f(O o) -> u256:
        return o.y
");
}

#[test]
fn a_struct_param_decodes_through_its_own_codec() {
    // The guard is the whole tuple's width, not one word: a static struct rides inline in the head rather than behind an offset.
    let src = "class P:
    u128 a
    u256 b

contract C:
    export fn f(P p) -> u256:
        return p.b
";
    assert_output_contains(src, "let param_p := gum_abi_st_P_cd(4)");
    assert_output_contains(src, "if lt(calldatasize(), 68) { revert(0, 0) }");
}

#[test]
fn an_array_return_of_a_non_scalar_is_rejected() {
    assert_compile_fails("contract C:\n    export fn f() -> [[u256]]:\n        return 1\n");
}

#[test]
fn a_scalar_array_across_the_abi_is_accepted() {
    // The rejection above must not catch the cases that do work.
    assert_compiles("use gum.defaults.Account\n\ncontract C:\n    export fn f([Account] a, [bool] b, [i8] c, [u256; 2] d) -> [i8]:\n        return c\n");
}

// --- Transient storage (EIP-1153) ---

#[test]
fn a_transient_scalar_uses_tstore_and_tload() {
    let src = "contract C:\n    transient u256 t\n\n    export fn set():\n        C.t = 1\n\n    export fn get() -> u256:\n        return C.t\n";
    assert_output_contains(src, "tstore(0, 1)");
    assert_output_contains(src, "tload(0)");
    assert_assembles(src);
}

#[test]
fn a_transient_field_and_a_persistent_one_may_share_a_slot_number() {
    // Two separate 256-bit keyspaces: transient slot 0 and persistent slot 0
    // are different locations, so the packers number them independently.
    let src = "contract C:\n    u256 a\n    transient u256 t\n\n    export fn go():\n        C.a = 1\n        C.t = 2\n";
    assert_output_contains(src, "sstore(0, 1)");
    assert_output_contains(src, "tstore(0, 2)");
    assert_assembles(src);
}

#[test]
fn transient_collections_get_their_own_helper_family() {
    // The storage kind is resolved in Rust, so each helper is emitted once per
    // kind it is used with, not selected at runtime.
    let src = "use gum.defaults.Account\n\ncontract C:\n    transient [u256] a\n    transient HashMap(Account, u256) m\n    transient String s\n\n    export fn go(Account w, String v):\n        C.a.push(1)\n        C.m[w] = 2\n        C.s = v\n";
    assert_output_contains(src, "function dpk_push_t(");
    assert_output_contains(src, "let n := tload(len_slot)");
    assert_output_contains(src, "function gum_sstr_store_t(");
    assert_output_contains(src, "tstore(gum_hash_slot(");
    assert_assembles(src);
}

#[test]
fn a_persistent_only_contract_emits_no_transient_helper() {
    // Only the kinds actually used are emitted, so nothing pays for transient
    // support by having it exist.
    let src = "contract C:\n    [u256] a\n\n    export fn go():\n        C.a.push(1)\n";
    assert_output_contains(src, "function dpk_push(");
    let (_, out) = run_gumc(src);
    assert!(!out.contains("dpk_push_t"), "no transient helper should be emitted:\n{}", out);
    assert!(!out.contains("tstore"), "no transient opcode should be emitted:\n{}", out);
}

#[test]
fn transient_fields_are_absent_from_the_storage_lock() {
    // The lock keeps a proxy's existing storage readable across an upgrade.
    // A transient field holds nothing across a transaction, so there is never
    // any storage for a moved slot to corrupt.
    let dir = std::env::temp_dir().join(format!("gum_tlock_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lock = dir.join("layout.json");
    // No solc needed: this only inspects the lock manifest, it never assembles bytecode.
    let (ok, out) = run_gumc_with_args(
        "contract C:\n    u256 kept\n    transient u256 scratch\n\n    export fn go():\n        C.kept = 1\n        C.scratch = 2\n",
        &["--lock", &lock.to_string_lossy()],
    );
    assert!(ok, "expected success, got:\n{}", out);
    let manifest = std::fs::read_to_string(&lock).unwrap();
    assert!(manifest.contains("kept"), "the persistent field must be committed:\n{}", manifest);
    assert!(!manifest.contains("scratch"), "a transient field must not be committed:\n{}", manifest);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transient_on_a_plain_class_field_is_rejected() {
    // Only a contract has storage; a plain class is a memory value, so the
    // modifier would name a keyspace the field never touches.
    assert_compile_fails("class P:\n    transient u256 x\n\ncontract C:\n    export fn go() -> u256:\n        var p = new P()\n        return p.x\n");
}

// The Yul between a selector case's opening brace and its matching close, for asserting on one function's dispatch.
fn case_body(yul: &str, name: &str) -> String {
    let marker = format!("/* {} */", name);
    let start = match yul.find(&marker) {
        Some(i) => i,
        None => return String::new(),
    };
    let rest = &yul[start..];
    let open = rest.find('{').unwrap_or(0);
    let mut depth = 0i32;
    for (i, c) in rest[open..].char_indices() {
        if c == '{' { depth += 1; }
        if c == '}' { depth -= 1; if depth == 0 { return rest[open..open + i + 1].to_string(); } }
    }
    rest.to_string()
}

#[test]
fn only_functions_that_call_out_carry_a_reentrancy_guard() {
    // A state-changing entry point that never hands control to another contract cannot be re-entered, so it needs no transient lock.
    // The guard must stay on anything that does call out, including transitively through an internal helper: dropping it there is a real reentrancy hole.
    let src = "use gum.defaults.Account
use gum.defaults.Message

contract V:
    HashMap(Account, u256) bal

    fn send_(Account to, u256 amt):
        to.transfer(amt)

    export fn touch(u256 x):
        V.bal[Message.sender()] = x

    export fn direct(u256 amt):
        V.bal[Message.sender()] -= amt
        Message.sender().transfer(amt)

    export fn indirect(u256 amt):
        V.bal[Message.sender()] -= amt
        V.send_(Message.sender(), amt)
";
    let (ok, out) = run_gumc(src);
    assert!(ok, "compile failed:
{}", out);
    // touch writes storage and calls nobody: no lock.
    let touch = case_body(&out, "touch");
    assert!(!touch.contains("tstore"), "touch should not be guarded:
{}", touch);
    // direct transfers: guarded.
    let direct = case_body(&out, "direct");
    assert!(direct.contains("tstore"), "direct must be guarded:
{}", direct);
    // indirect transfers through a helper: still guarded (transitive).
    let indirect = case_body(&out, "indirect");
    assert!(indirect.contains("tstore"), "indirect must be guarded through its helper:
{}", indirect);
}
