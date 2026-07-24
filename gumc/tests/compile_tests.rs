use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

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

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

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
    assert!(
        ok,
        "expected bytecode assembly to succeed, got:\n{}",
        output
    );
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
    assert!(
        !ok,
        "expected compile failure, but it succeeded:\n{}",
        output
    );
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
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

#[test]
fn token_gum_compiles() {
    assert_compiles(&read_repo_file("examples/token.gum"));
}

#[test]
fn amm_gum_compiles() {
    assert_compiles(&read_repo_file("examples/amm.gum"));
}

fn yul_only(out: &str) -> String {
    out.lines()
        .filter(|l| !l.starts_with("--> Compiling"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn compiling_the_same_source_twice_gives_the_same_output() {
    let src = read_repo_file("examples/amm.gum");
    let (ok, first) = run_gumc(&src);
    assert!(ok, "amm failed to compile:\n{}", first);
    let first = yul_only(&first);
    for i in 0..3 {
        let (ok2, again) = run_gumc(&src);
        assert!(ok2, "amm failed to compile on run {}", i);
        assert_eq!(
            first,
            yul_only(&again),
            "compile {} differed from the first: output is not reproducible",
            i
        );
    }
}

#[test]
fn the_standard_library_module_compiles() {
    let source = include_str!("../../std/defaults.gum");
    let (ok, output) = run_gumc(source);
    assert!(ok, "std/defaults.gum failed to compile:\n{}", output);
}

#[test]
fn every_standard_symbol_imports_on_its_own() {
    for sym in [
        "Account",
        "Message",
        "Block",
        "String",
        "Bytes",
        "HashMap",
        "Vec",
        "Serializable",
        "Hashable",
        "Crypto",
        "keccak256",
        "ecrecover",
    ] {
        let src = format!(
            "use gum.defaults.{}\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n",
            sym
        );
        let (ok, output) = run_gumc(&src);
        assert!(ok, "use gum.defaults.{} failed:\n{}", sym, output);
    }
}

#[test]
fn operator_precedence_groups_comparison_below_logical_or() {
    assert_output_contains(
        "contract App:\n    export fn f(u256 a, u256 b, u256 c, u256 d) -> bool:\n        return a == b || c == d\n",
        "or(eq(a, b), eq(c, d))",
    );
}

#[test]
fn arithmetic_binds_tighter_than_comparison() {
    assert_output_contains(
        "contract App:\n    export fn f(u256 a, u256 b, u256 c) -> bool:\n        return a + b < c\n",
        "lt(checked_add(a, b, not(0)), c)",
    );
}

#[test]
fn account_param_abi_encodes_as_address() {
    assert_output_contains(
        "use gum.defaults.Account\n\ncontract App:\n    export fn take(Account who) -> u256:\n        return 0\n",
        "\"address\"",
    );
}

#[test]
fn narrow_int_literal_is_masked() {
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
    assert_output_contains(
        "class Counter:\n    u256 value\n\n    fn new(self, u256 start):\n        self.value = start\n\n    fn get(self) -> u256:\n        return self.value\n\ncontract App:\n    export fn make(u256 start) -> u256:\n        mut Counter c = Counter.new(start)\n        return c.get()\n",
        "__ctor_new_Counter",
    );
}

#[test]
fn cheatcode_sender_changes_msg_sender() {
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping sender check: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();
    let src = "use gum.defaults.hashable\nuse gum.defaults.Message\n\ncontract Target:\n    Account seen\n    export fn record():\n        Target.seen = Message.sender()\n    export fn who() -> Account:\n        return Target.seen\n\ninterface ITarget:\n    fn record()\n    fn who() -> Account\n\ncontract SenderTest:\n    [Test]\n    fn sender_sets_msg_sender():\n        var t = Target.new()\n        Vm.sender = 0x00000000000000000000000000000000000000AA\n        ITarget(t).record()\n        assert(ITarget(t).who() == 0x00000000000000000000000000000000000000AA, \"sender not set\")\n    [Test]\n    fn without_set_sender_differs():\n        var t = Target.new()\n        ITarget(t).record()\n        assert(ITarget(t).who() != 0x00000000000000000000000000000000000000AA, \"unexpected sender\")\n";
    let (ok, output) = run_gumc_with_args(src, &["--test", "--solc", &solc_arg]);
    assert!(ok, "sender tests should pass:\n{}", output);
    assert!(
        output.contains("ok    sender_sets_msg_sender"),
        "Vm.sender did not take effect:\n{}",
        output
    );
    assert!(
        output.contains("2 tests, 2 passed"),
        "expected both to pass:\n{}",
        output
    );
}

#[test]
fn scoped_try_catches_internal_revert_and_rolls_back() {
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
    assert!(
        output.contains("2 tests, 2 passed"),
        "expected both to pass:\n{}",
        output
    );
}

#[test]
fn test_runner_reports_pass_and_fail() {
    let solc = match find_solc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test runner check: no solc");
            return;
        }
    };
    let solc_arg = solc.to_string_lossy().into_owned();

    let src = "use gum.defaults.hashable\n\ncontract Suite:\n    fn helper() -> u256:\n        return 1\n\n    [Test]\n    fn passes():\n        assert(self.helper() == 1, \"nope\")\n\n    [Test]\n    fn fails():\n        assert(1 == 2, \"boom\")\n";
    let (ok, output) = run_gumc_with_args(src, &["--test", "--solc", &solc_arg]);
    assert!(
        !ok,
        "a failing test must make gumc exit non-zero:\n{}",
        output
    );
    assert!(
        output.contains("ok    passes"),
        "expected a pass line:\n{}",
        output
    );
    assert!(
        output.contains("FAIL  fails"),
        "expected a fail line:\n{}",
        output
    );
    assert!(
        output.contains("\"boom\""),
        "expected the revert reason:\n{}",
        output
    );
    assert!(
        output.contains("1 passed, 1 failed"),
        "expected a summary:\n{}",
        output
    );
    assert!(
        !output.contains("helper"),
        "a plain fn helper must not run as a test:\n{}",
        output
    );
}

#[test]
fn discarded_call_return_is_popped() {
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
        "use gum.defaults.vec\n\ncontract App:\n    export fn foo() -> u256:\n        mut Vec(u256) v = Vec(u256).new()\n        v = v.push(10)\n        v = v.push(20)\n        return v.get(1)\n",
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

#[test]
fn nonpayable_callvalue_guard_is_hoisted_once() {
    let (ok, output) = run_gumc(
        "contract App:\n    export fn a(u256 x) -> u256:\n        return x\n\n    export fn b(u256 y) -> u256:\n        return y\n",
    );
    assert!(ok, "{}", output);
    let guards = output.matches("if callvalue()").count();
    assert_eq!(
        guards, 1,
        "expected a single hoisted callvalue guard, got {}:\n{}",
        guards, output
    );
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
    assert!(
        output.contains(":= 7"),
        "expected 3+4 folded to 7:\n{}",
        output
    );
    assert!(
        !output.contains("checked_add(3, 4"),
        "constant add should not emit a runtime check:\n{}",
        output
    );
}

#[test]
fn unprovable_constant_overflow_keeps_the_runtime_check() {
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

#[test]
fn calldatasize_guard_is_present_once() {
    let (ok, output) =
        run_gumc("contract App:\n    export fn a(u256 x) -> u256:\n        return x\n");
    assert!(ok, "{}", output);
    assert!(
        output.contains("if lt(calldatasize(), 4)"),
        "missing calldatasize guard:\n{}",
        output
    );
}

#[test]
fn default_reverts_are_blank() {
    let (ok, output) =
        run_gumc("contract App:\n    export fn f(u256 a, u256 b) -> u256:\n        return a + b\n");
    assert!(ok, "{}", output);
    assert!(
        !output.contains("4e487b71"),
        "default build must not carry Panic data:\n{}",
        output
    );
}

#[test]
fn rich_reverts_flag_emits_panic_codes() {
    let (ok, output) = run_gumc_with_args(
        "contract App:\n    export fn f(u256 a, u256 b) -> u256:\n        return a / b\n",
        &["--rich-reverts"],
    );
    assert!(ok, "{}", output);
    assert!(
        output.contains("mstore(0, shl(224, 0x4e487b71))"),
        "expected Panic selector:\n{}",
        output
    );
    assert!(
        output.contains("mstore(4, 0x12)"),
        "expected div-by-zero panic code 0x12:\n{}",
        output
    );
}

#[test]
fn indexed_event_uses_canonical_erc20_transfer_topic() {
    let src = read_repo_file("examples/token.gum");
    assert_output_contains(
        &src,
        "log3(blob, alen, 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef, sender, to)",
    );

    assert_output_contains(&src, "let alen := 32");
}

#[test]
fn non_indexed_event_stays_in_data() {
    let src = read_repo_file("examples/token.gum");
    assert_output_contains(&src, "log2(blob, alen,");
    assert_output_contains(&src, "let alen := 32");
}

#[test]
fn events_still_assemble_to_bytecode() {
    assert_assembles(&read_repo_file("examples/token.gum"));
    assert_assembles(&read_repo_file("examples/amm.gum"));
}

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
    assert_assembles(
        "use gum.defaults.vec\n\ncontract App:\n    export fn crunch(u256 seed) -> u256:\n        mut [u8; 4] xs = [1, 2, 3, 4]\n        mut u256 total = seed\n        for x in xs:\n            total += x\n        mut i256 signed = -3\n        mut Vec(u256) v = Vec(u256).new()\n        v = v.push(total)\n        return v.get(0) % 7 + total ** 2\n",
    );
}

#[test]
fn multiple_independent_semantic_errors_are_all_reported() {
    let (ok, output) = run_gumc(
        "contract App:\n    export fn broken_one() -> u256:\n        return missing_one\n\n    export fn broken_two() -> u256:\n        return missing_two\n",
    );
    assert!(!ok);
    assert!(
        output.contains("missing_one"),
        "missing first error:\n{}",
        output
    );
    assert!(
        output.contains("missing_two"),
        "missing second error:\n{}",
        output
    );
    assert!(
        output.contains("2 semantic error"),
        "expected an error count summary:\n{}",
        output
    );
}

#[test]
fn semantic_errors_carry_exact_line_and_col() {
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
    assert_compile_fails(
        "contract App:\n    export fn foo() -> u256:\n        bool b = 5\n        return 1\n",
    );
}

#[test]
fn undefined_identifier_is_rejected() {
    assert_compile_fails(
        "contract App:\n    export fn foo() -> u256:\n        return not_a_real_thing\n",
    );
}

#[test]
fn missing_return_on_all_paths_is_rejected() {
    assert_compile_fails(
        "contract App:\n    export fn foo(bool flag) -> u256:\n        if flag:\n            return 1\n",
    );
}

#[test]
fn friendly_keywords_compile_and_assemble() {
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
    assert_compiles(
        "contract App:\n    export fn f() -> bool:\n        var x = 1 == 1\n        return x\n",
    );
}

#[test]
fn var_without_initializer_is_rejected() {
    assert_compile_fails("contract App:\n    export fn f():\n        var x\n");
}

#[test]
fn const_infers_and_is_immutable() {
    assert_compiles(
        "contract App:\n    export fn f() -> u256:\n        const x = 40 + 2\n        return x\n",
    );
    assert_compile_fails(
        "contract App:\n    export fn f() -> u256:\n        const x = 42\n        x = 43\n        return x\n",
    );
}

#[test]
fn var_is_immutable_by_default_mut_opts_in() {
    assert_compile_fails(
        "contract App:\n    export fn f() -> u256:\n        var x = 42\n        x = 43\n        return x\n",
    );

    assert_compiles(
        "contract App:\n    export fn f() -> u256:\n        mut var x = 42\n        x = 43\n        return x\n",
    );
}

#[test]
fn global_keyword_is_gone() {
    assert_compile_fails("global fn f() -> u256:\n    return 1\n");
    assert_compile_fails("global class S:\n    u256 a\n");
}

#[test]
fn bare_fn_is_not_externally_callable() {
    let (ok, out) = run_gumc(
        "fn helper(u256 x) -> u256:\n    return x + 1\n\ncontract App:\n    export fn pub() -> u256:\n        return helper(1)\n",
    );
    assert!(ok, "expected compile success:\n{}", out);
    assert!(
        !out.contains("\"helper\""),
        "internal fn must not appear in the ABI:\n{}",
        out
    );
    assert!(
        out.contains("\"pub\""),
        "exported fn must appear in the ABI:\n{}",
        out
    );
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
    assert!(
        ok,
        "expected successful compile, got:
{}",
        out
    );
    assert!(
        out.contains("\"payable\""),
        "payable fn must be marked payable in the ABI:
{}",
        out
    );
    assert!(
        out.contains("\"nonpayable\""),
        "non-payable fn must stay nonpayable in the ABI:
{}",
        out
    );
}

#[test]
fn nonpayable_guard_is_hoisted_only_when_nothing_is_payable() {
    let (ok, out) = run_gumc(
        "contract S:
    u256 t

    export fn a():
        S.t = 1

    export fn b():
        S.t = 2
",
    );
    assert!(
        ok,
        "expected successful compile, got:
{}",
        out
    );
    assert_eq!(
        out.matches("if callvalue() { revert(0, 0) }").count(),
        1,
        "expected exactly one hoisted callvalue guard, got:
{}",
        out
    );

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
    assert!(
        ok,
        "expected successful compile, got:
{}",
        out
    );
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
    assert_compiles(
        "contract App:\n    export fn f(u256 x):\n        if x == 0:\n            return\n        return\n",
    );

    assert_compile_fails("contract App:\n    export fn f() -> u256:\n        return\n");

    assert_compile_fails("contract App:\n    export fn f():\n        return 5\n");
}

#[test]
fn reentrancy_guard_sees_calls_the_semantic_pass_could_have_missed() {
    assert_output_contains(
        "use gum.defaults.Account\ninterface IERC20:\n    fn transfer(Account to, u256 amount) -> bool\n\ncontract S:\n    u256 t\n\n    export fn go(Account tok):\n        if IERC20(tok).transfer(tok, 1):\n            S.t = 1\n",
        "tload",
    );

    assert_output_contains(
        "contract S:\n    u256 t\n\n    export fn go():\n        S.t = 1\n        unsafe:\n            let ok := call(gas(), 0x1234, 0, 0, 0, 0, 0)\n",
        "tload",
    );
}

#[test]
fn no_reentrancy_guard_when_nothing_can_yield() {
    let (ok, out) = run_gumc(
        "contract S:\n    u256 t\n\n    export fn a():\n        S.t = 1\n\n    export fn b() -> u256:\n        return S.t\n",
    );
    assert!(ok, "expected successful compile, got:\n{}", out);
    assert!(
        !out.contains("tload(0x"),
        "a contract with no external calls must emit no guard:\n{}",
        out
    );
}

#[test]
fn a_persistent_only_contract_has_no_transient_opcodes_in_its_bytecode() {
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
        .find(|l| {
            l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit())
        })
        .expect("no bytecode");
    let code = hex_to_bytes(&hex[2..]);
    let ops = storage_opcodes(&code);
    assert!(
        ops.contains(&"SSTORE") && ops.contains(&"SLOAD"),
        "expected persistent access, got {:?}",
        ops
    );
    assert!(
        !ops.contains(&"TSTORE") && !ops.contains(&"TLOAD"),
        "a contract with no transient fields must contain no transient opcode, got {:?}",
        ops
    );
}

#[test]
fn a_transient_only_contract_never_touches_persistent_storage() {
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
        .find(|l| {
            l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit())
        })
        .expect("no bytecode");
    let code = hex_to_bytes(&hex[2..]);
    let ops = storage_opcodes(&code);

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

#[test]
fn super_resolves_to_the_overridden_method() {
    let (ok, out) = run_gumc(
        "class Base:\n    u256 v\n\n    fn label(self) -> u256:\n        return self.v + 10\n\n\
         [Base]\nclass Child:\n    fn label(self) -> u256:\n        return super.label() + 5\n\n\
         contract C:\n    u256 z\n\n    export fn r():\n        C.z = 1\n",
    );
    assert!(ok, "expected success, got:\n{}", out);
    assert!(
        out.contains("function Child_super_label(self)"),
        "parent body not kept:\n{}",
        out
    );
    assert!(
        out.contains("Child_super_label(self)"),
        "super call not emitted:\n{}",
        out
    );
}

#[test]
fn super_without_an_override_is_rejected() {
    let (ok, out) = run_gumc(
        "class Base:\n    u256 v\n\n    fn a(self) -> u256:\n        return 1\n\n\
         [Base]\nclass Child:\n    fn b(self) -> u256:\n        return super.a()\n\n\
         contract C:\n    u256 z\n\n    export fn r():\n        C.z = 1\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("has nothing to call"),
        "unexpected error:\n{}",
        out
    );
}

#[test]
fn super_outside_a_method_is_rejected() {
    let (ok, out) = run_gumc(
        "fn helper() -> u256:\n    return super.foo()\n\n\
         contract C:\n    u256 z\n\n    export fn r():\n        C.z = 1\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("only available inside a method"),
        "unexpected error:\n{}",
        out
    );
}

fn const_field_src() -> &'static str {
    "contract Cfg:\n    const Account owner\n    const u256 cap\n    u256 counter\n\n    \
     fn new(Account o, u256 c):\n        Cfg.owner = o\n        Cfg.cap = c\n\n    \
     export fn get_owner() -> Account:\n        return Cfg.owner\n\n    \
     export fn get_cap() -> u256:\n        return Cfg.cap\n\n    \
     export fn bump():\n        Cfg.counter = Cfg.counter + 1\n"
}

#[test]
fn a_const_field_is_read_from_code_and_never_from_storage() {
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
        .find(|l| {
            l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit())
        })
        .expect("no bytecode");
    let ops = storage_opcodes(&hex_to_bytes(&hex[2..]));
    let sloads = ops.iter().filter(|o| **o == "SLOAD").count();
    assert_eq!(sloads, 1, "expected only counter's SLOAD, got {:?}", ops);
}

#[test]
fn a_const_field_from_a_constructor_arg_is_patched_at_deploy() {
    let (ok, out) = run_gumc(const_field_src());
    assert!(ok, "expected success, got:\n{}", out);

    assert!(
        out.contains(r#"setimmutable(0, "Cfg_owner", _immv_owner)"#),
        "no setimmutable:\n{}",
        out
    );
    assert!(
        out.contains(r#"setimmutable(0, "Cfg_cap", _immv_cap)"#),
        "no setimmutable:\n{}",
        out
    );

    assert!(
        out.contains(r#"loadimmutable("Cfg_owner")"#),
        "no loadimmutable:\n{}",
        out
    );
}

#[test]
fn a_const_field_occupies_no_storage_slot() {
    let (ok, out) = run_gumc(const_field_src());
    assert!(ok, "expected success, got:\n{}", out);
    assert!(
        out.contains("sload(0x0)") || out.contains("sload(0)"),
        "counter should be slot 0:\n{}",
        out
    );
}

#[test]
fn a_const_field_is_absent_from_the_storage_lock() {
    let dir = std::env::temp_dir().join(format!("gum_imm_lock_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let lock = dir.join("layout.json");
    let lock_arg = lock.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(const_field_src(), &["--lock", &lock_arg]);
    assert!(ok, "expected success, got:\n{}", out);
    let manifest = std::fs::read_to_string(&lock).expect("lockfile");
    assert!(
        manifest.contains("counter"),
        "the real storage field should be committed:\n{}",
        manifest
    );
    assert!(
        !manifest.contains("owner"),
        "immutable 'owner' must not be in the lock:\n{}",
        manifest
    );
    assert!(
        !manifest.contains("\"cap\""),
        "immutable 'cap' must not be in the lock:\n{}",
        manifest
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn bytecode_of(src: &str) -> Option<String> {
    let solc = find_solc()?;
    let solc_arg = solc.to_string_lossy().into_owned();
    let (ok, out) = run_gumc_with_args(src, &["--bytecode", "--solc", &solc_arg]);
    assert!(ok, "expected success, got:\n{}", out);
    Some(
        out.lines()
            .map(str::trim)
            .find(|l| {
                l.starts_with("0x") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_hexdigit())
            })
            .expect("no bytecode")
            .to_string(),
    )
}

#[test]
fn a_const_field_the_compiler_can_evaluate_costs_nothing() {
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
    assert_eq!(
        folded, literal,
        "a const the compiler can evaluate must cost exactly nothing"
    );
}

#[test]
fn a_const_field_the_compiler_cannot_evaluate_is_patched_at_deploy() {
    let (ok, out) = run_gumc(
        "contract A:\n    const u256 cap\n\n    fn new(u256 c):\n        A.cap = c\n\n    \
         export fn get() -> u256:\n        return A.cap\n",
    );
    assert!(ok, "expected success, got:\n{}", out);
    assert!(
        out.contains(r#"setimmutable(0, "A_cap", _immv_cap)"#),
        "expected a deploy-time patch:\n{}",
        out
    );
    assert!(
        out.contains(r#"loadimmutable("A_cap")"#),
        "expected a code read:\n{}",
        out
    );
}

#[test]
fn a_const_field_is_only_folded_when_it_is_unconditional() {
    for (label, body) in [
        (
            "assigned in a branch",
            "        if c:\n            A.cap = 100\n        else:\n            A.cap = 200",
        ),
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
    let (ok, out) = run_gumc(
        "contract C:\n    const u256 a\n\n    export fn g() -> u256:\n        return C.a\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("has no fn new()"),
        "unexpected error:\n{}",
        out
    );
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

fn ctor_body(body: &str) -> String {
    format!(
        "enum Error:\n    Bad()\n\ncontract C:\n    const u256 a\n\n    fn new(u256 x, bool c):\n{}\n\n    \
         export fn g() -> u256:\n        return C.a\n",
        body
    )
}

#[test]
fn a_const_field_assigned_on_only_some_paths_is_rejected() {
    for (label, body) in [
        ("if with no else", "        if c:\n            C.a = x"),
        (
            "only inside a while",
            "        while c:\n            C.a = x",
        ),
        (
            "only inside a for",
            "        for i in [1, 2]:\n            C.a = i",
        ),
    ] {
        let (ok, out) = run_gumc(&ctor_body(body));
        assert!(
            !ok,
            "{label}: expected a compile error, got success:\n{}",
            out
        );
        assert!(
            out.contains("not on every path"),
            "{label}: expected a path-coverage error, got:\n{}",
            out
        );
    }
}

#[test]
fn a_const_field_assigned_on_every_path_is_accepted() {
    for (label, body) in [
        ("unconditional", "        C.a = x"),
        (
            "both branches assign",
            "        if c:\n            C.a = x\n        else:\n            C.a = 1",
        ),
        (
            "else diverges",
            "        if c:\n            C.a = x\n        else:\n            revert Error.Bad()",
        ),
        (
            "if diverges",
            "        if c:\n            revert Error.Bad()\n        else:\n            C.a = x",
        ),
        (
            "assigned then re-assigned",
            "        C.a = 1\n        if c:\n            C.a = x",
        ),
    ] {
        let (ok, out) = run_gumc(&ctor_body(body));
        assert!(ok, "{label}: expected success, got:\n{}", out);
    }
}

#[test]
fn reading_a_const_field_inside_the_constructor_is_rejected() {
    let (ok, out) = run_gumc(&ctor_body("        C.a = x\n        var y = C.a + 1"));
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("reads const field 'a'"),
        "unexpected error:\n{}",
        out
    );
}

#[test]
fn assigning_a_const_field_outside_the_constructor_is_rejected() {
    let (ok, out) = run_gumc(
        "contract C:\n    const u256 a\n\n    fn new(u256 x):\n        C.a = x\n\n    \
         export fn s(u256 y):\n        C.a = y\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("cannot be written afterwards"),
        "unexpected error:\n{}",
        out
    );
}

#[test]
fn a_const_field_on_a_plain_class_is_rejected() {
    let (ok, out) = run_gumc(
        "class P:\n    const u256 a\n\ncontract C:\n    u256 z\n\n    export fn g():\n        C.z = 1\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("cannot be const"),
        "unexpected error:\n{}",
        out
    );
}

#[test]
fn a_field_cannot_be_both_transient_and_const() {
    let (ok, out) = run_gumc(
        "contract C:\n    transient const u256 a\n\n    fn new(u256 x):\n        C.a = x\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("cannot be both transient and const"),
        "unexpected error:\n{}",
        out
    );
}

fn abi_of(out: &str) -> serde_json::Value {
    let start = out.find("ABI JSON Generated:").expect("no ABI in output");
    let open = out[start..].find('[').expect("no ABI array") + start;

    let close = out[open..].find("\n]").expect("unterminated ABI array") + open + 2;
    serde_json::from_str(&out[open..close]).expect("ABI is not valid JSON")
}

fn keccak_hex(s: &str) -> String {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(s.as_bytes());
    k.finalize(&mut out);
    format!(
        "0x{}",
        out.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    )
}

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
    let src = std::fs::read_to_string(repo_root().join("examples/token.gum")).expect("token.gum");
    let (ok, out) = run_gumc(&src);
    assert!(ok, "expected success, got:\n{}", out);

    let abi = abi_of(&out);
    let events: Vec<&serde_json::Value> = abi
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "event")
        .collect();
    assert_eq!(
        events.len(),
        2,
        "token.gum logs two events, got {:?}",
        events
    );

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
    const ERC20_TRANSFER: &str =
        "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    assert_eq!(
        keccak_hex("Transfer(address,address,uint256)"),
        ERC20_TRANSFER
    );

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
    assert_eq!(
        abi_event_signature(transfer),
        "Transfer(address,address,uint256)"
    );
    assert!(
        out.contains(ERC20_TRANSFER),
        "canonical ERC20 Transfer topic0 not in the Yul"
    );
}

#[test]
fn event_abi_marks_indexed_fields_and_names_them_from_the_call_site() {
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
                i["indexed"]
                    .as_bool()
                    .expect("event input must carry indexed"),
            )
        })
        .collect();
    assert_eq!(
        fields,
        vec![
            ("sender", "address", true),
            ("to", "address", true),
            ("amount", "uint256", false)
        ]
    );
}

#[test]
fn an_event_entry_carries_no_outputs_or_state_mutability() {
    let src = std::fs::read_to_string(repo_root().join("examples/token.gum")).expect("token.gum");
    let (ok, out) = run_gumc(&src);
    assert!(ok, "expected success, got:\n{}", out);

    for e in abi_of(&out).as_array().unwrap() {
        if e["type"] != "event" {
            continue;
        }
        assert!(e.get("outputs").is_none(), "event has outputs: {}", e);
        assert!(
            e.get("stateMutability").is_none(),
            "event has stateMutability: {}",
            e
        );
        assert_eq!(e["anonymous"], serde_json::json!(false));
    }

    let f = abi_of(&out)
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "function")
        .cloned()
        .expect("no function entry");
    for i in f["inputs"].as_array().unwrap() {
        assert!(
            i.get("indexed").is_none(),
            "function input has indexed: {}",
            i
        );
    }
}

#[test]
fn logging_one_event_with_two_different_shapes_is_rejected() {
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
    let (ok, out) = run_gumc(
        "enum L:\n    E\n\ncontract T:\n    export fn a(Account x):\n        log(L.E, indexed(x))\n\n    \
         export fn b(Account y):\n        log(L.E, y)\n",
    );
    assert!(!ok, "expected a compile error, got success:\n{}", out);
    assert!(
        out.contains("two different shapes"),
        "expected a shape conflict, got:\n{}",
        out
    );
}

#[test]
fn the_same_event_from_two_sites_with_different_arg_names_is_one_entry() {
    let src = "enum L:\n    E\n\ncontract T:\n    \
         export fn a(Account from, Account to, u256 v):\n        log(L.E, indexed(from), indexed(to), v)\n\n    \
         export fn b(Account src, Account dst, u256 amt):\n        log(L.E, indexed(src), indexed(dst), amt)\n";
    assert_output_contains(src, "\"type\": \"event\"");
    let (ok, out) = run_gumc(src);
    assert!(ok, "arg names must not make one event two shapes, got:\n{}", out);
    assert_eq!(
        out.matches("\"name\": \"E\"").count(),
        1,
        "event E must appear exactly once in the ABI, got:\n{}",
        out
    );
}

#[test]
fn the_std_token_library_compiles() {
    assert_compiles(include_str!("../../std/tokens/erc20.gum"));
    assert_compiles(include_str!("../../std/tokens/erc721.gum"));
}

#[test]
fn a_contract_that_logs_nothing_has_no_event_entries() {
    let (ok, out) = run_gumc("contract S:\n    u256 t\n\n    export fn a():\n        S.t = 1\n");
    assert!(ok, "expected success, got:\n{}", out);
    assert!(
        !abi_of(&out)
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "event"),
        "unexpected event entry:\n{}",
        out
    );
}

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
    assert_compile_fails(
        "contract App:\n    export fn f(u256 x):\n        if x:\n            return\n",
    );
    assert_compile_fails(
        "contract App:\n    export fn f():\n        if not_a_thing:\n            return\n",
    );
}

#[test]
fn eip7702_delegation_decode_has_the_right_shape() {
    let src = "use gum.defaults.Account\n\ncontract App:\n    export fn d(Account a) -> Account:\n        return a.delegated_to()\n";
    assert_output_contains(src, "eq(extcodesize(a), 23)");
    assert_output_contains(src, "extcodecopy(a, p, 0, 23)");
    assert_output_contains(src, "eq(shr(232, mload(p)), 0xef0100)");

    assert_output_contains(
        src,
        "and(shr(72, mload(p)), 0xffffffffffffffffffffffffffffffffffffffff)",
    );
}

#[test]
fn p256_verify_calls_the_precompile_at_0x100() {
    let src = "use gum.defaults.crypto\n\ncontract App:\n    export fn v(u256 h, u256 r, u256 s, u256 qx, u256 qy) -> bool:\n        return Crypto.verify_p256(h, r, s, qx, qy)\n";
    assert_output_contains(src, "staticcall(gas(), 0x100, p, 160, add(p, 160), 32)");

    assert_output_contains(src, "eq(returndatasize(), 32)");
}

#[test]
fn p256_verify_arity_is_checked() {
    assert_compile_fails(
        "use gum.defaults.crypto\n\ncontract App:\n    export fn v(u256 h) -> bool:\n        return Crypto.verify_p256(h)\n",
    );
}

#[test]
fn delete_rejects_a_whole_hashmap() {
    assert_compile_fails(
        "use gum.defaults.Account\n\ncontract D:\n    HashMap(Account, u256) bal\n\n    export fn bad():\n        delete D.bal\n",
    );
}

#[test]
fn delete_rejects_an_immutable_local() {
    assert_compile_fails(
        "contract App:\n    export fn bad():\n        var x = 5\n        delete x\n",
    );
}

#[test]
fn delete_rejects_a_computed_expression() {
    assert_compile_fails(
        "contract App:\n    export fn bad(u256 a, u256 b):\n        delete a + b\n",
    );
}

#[test]
fn delete_on_a_packed_field_preserves_its_slot_mates() {
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

#[test]
fn inherited_fields_come_before_the_child_s_own() {
    let src = "class Base:\n    u256 a\n    u256 b\n\n[Base]\ncontract C:\n    u256 d\n\n    export fn get() -> u256:\n        return C.d\n";

    assert_output_contains(src, "sload(2)");
}

#[test]
fn a_child_inherits_its_parent_s_methods() {
    let src = "class Base:\n    u256 a\n\n    fn twice(self) -> u256:\n        return self.a * 2\n\n[Base]\ncontract C:\n    u256 z\n\n    export fn go() -> u256:\n        return C.twice()\n";
    assert_output_contains(src, "function C_twice()");
}

#[test]
fn a_child_method_overrides_its_parent_s() {
    let src = "class Base:\n    fn label() -> u256:\n        return 1\n\n[Base]\nclass Mid:\n    fn label() -> u256:\n        return 2\n\n[Mid]\ncontract C:\n    u256 z\n\n    export fn go() -> u256:\n        return C.label()\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);

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
    let src = "class Base:\n    u256 a\n\n    fn new(self, u256 v):\n        self.a = v\n\n[Base]\ncontract C:\n    u256 z\n";
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
    assert_compile_fails(
        "interface IThing:\n    fn ping(u256 x) -> bool\n\n[IThing]\nclass Impl:\n    u256 v\n",
    );
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
    let src = "use gum.defaults.Serializable\n\n[Serializable]\nclass A:\n    u256 x\n\n    fn new(self, u256 v):\n        self.x = v\n\n[A]\nclass B:\n    u256 y\n\ncontract App:\n    export fn go() -> [u8]:\n        var b = B.new(7)\n        return b.serialize()\n";
    assert_output_contains(src, "function B_serialize");
}

#[test]
fn imports_are_transitive() {
    assert_compiles(
        "use gum.defaults.Account\n\ncontract App:\n    export fn go(Account a) -> u256:\n        return a.balance()\n",
    );
}

#[test]
fn the_standard_library_needs_no_search_path() {
    let src = "use gum.defaults.String\n\ncontract C:\n    export fn f(String s) -> u256:\n        return s.length\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success with no base dir, got:\n{}", output);
    assert!(
        output.contains("Loading gum.defaults"),
        "String should resolve from the embedded table, got:\n{}",
        output
    );
}

#[test]
fn an_unresolvable_import_is_an_error() {
    let (ok, output) = run_gumc(
        "use gum.defaults.Nonsense\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n",
    );
    assert!(!ok, "an unknown std module must fail, got:\n{}", output);
    assert!(
        output.contains("has no 'Nonsense'"),
        "expected a module error naming the symbol, got:\n{}",
        output
    );
    let (ok3, out3) = run_gumc(
        "use gum.nope.Thing\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n",
    );
    assert!(!ok3, "an unknown std module must fail, got:\n{}", out3);
    assert!(
        out3.contains("does not name anything in the standard library"),
        "expected a module error, got:\n{}",
        out3
    );

    let (ok2, out2) =
        run_gumc("use not_here\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n");
    assert!(!ok2, "a missing local module must fail, got:\n{}", out2);
    assert!(
        out2.contains("cannot read module"),
        "expected a read error, got:\n{}",
        out2
    );
}

#[test]
fn a_module_imported_twice_is_only_loaded_once() {
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

#[test]
fn every_broken_function_in_a_contract_is_reported_not_just_the_first() {
    let src = "contract C:\n    u256 x\n\n    export fn one() -> u256:\n        retrn C.x\n\n    export fn two() -> u256:\n        return C.x\n\n    export fn three(u256 a) -> u256:\n        return a +\n\n    export fn four() -> u256:\n        return 4\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(
        output.contains("2 syntax errors found"),
        "expected both errors, got:\n{}",
        output
    );

    assert!(
        output.contains("--> 5:"),
        "first error should be on line 5, got:\n{}",
        output
    );
    assert!(
        output.contains("--> 11:"),
        "second error should be on line 11, got:\n{}",
        output
    );
}

#[test]
fn every_broken_statement_in_one_function_is_reported_not_just_the_first() {
    let src = "contract C:\n    export fn f(u256 a) -> u256:\n        retrn a\n        mut u256 r = 0\n        r = r @@ 1\n        return r\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(
        output.contains("2 syntax errors found"),
        "expected both statement errors, got:\n{}",
        output
    );
    assert!(
        output.contains("--> 3:"),
        "first error should be on line 3, got:\n{}",
        output
    );
    assert!(
        output.contains("--> 5:"),
        "second error should be on line 5, got:\n{}",
        output
    );
}

#[test]
fn a_bad_statement_and_a_bad_signature_still_leave_valid_functions_alone() {
    let src = "contract C:\n    export fn bad() -> u256:\n        return @\n\n    export fn good() -> u256:\n        return 1\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(
        output.contains("1 syntax error"),
        "expected exactly one error, got:\n{}",
        output
    );
    assert!(
        output.contains("--> 3:"),
        "error should be on line 3, got:\n{}",
        output
    );
}

#[test]
fn a_broken_declaration_does_not_hide_a_later_one_of_a_different_kind() {
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
    let src = "contract C\n    u256 x\n\n    export fn a() -> u256:\n        return 1\n";
    let (ok, output) = run_gumc(src);
    assert!(!ok, "expected failure, got:\n{}", output);
    assert!(
        output.contains("Indentation error"),
        "expected an indent error, got:\n{}",
        output
    );
}

#[test]
fn a_string_literal_containing_braces_does_not_split_a_declaration() {
    assert_compiles(
        "contract App:\n    export fn f() -> String:\n        return \"a { b } c ; d\"\n",
    );
}

#[test]
fn a_comment_containing_braces_does_not_split_a_declaration() {
    assert_compiles(
        "contract App:\n    export fn f() -> u256:\n        // } ; { not real structure\n        return 1\n",
    );
}

#[test]
fn an_fstring_with_interpolation_does_not_split_a_declaration() {
    assert_compiles(
        "contract App:\n    export fn f(u256 n) -> String:\n        return f\"n is {n}; ok\"\n",
    );
}

#[test]
fn an_unsafe_block_s_nested_braces_do_not_split_a_declaration() {
    let src = "contract App:\n    export fn f(u256 a) -> u256:\n        mut u256 r = 0\n        unsafe:\n            for { let i := 0 } lt(i, a) { i := add(i, 1) } {\n                r := add(r, i)\n            }\n        return r\n";
    assert_compiles(src);
}

#[test]
fn trailing_garbage_after_a_declaration_is_not_silently_dropped() {
    assert_compile_fails("use gum.defaults.Account extra\n");
}

#[test]
fn copying_a_whole_storage_array_of_scalars_is_allowed() {
    assert_compiles(
        "contract C:\n    [u256] arr\n\n    export fn ok() -> u256:\n        var a = C.arr\n        return a[0]\n",
    );
}

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
    assert_compiles(
        "contract C:\n    [u256] arr\n    u256 total\n\n    export fn ok() -> u256:\n        C.arr.push(1)\n        C.total = 0\n        for x in C.arr:\n            C.total = C.total + x\n        C.total = C.total + C.arr[0] + C.arr.length\n        C.arr.pop()\n        return C.total\n",
    );
}

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
    let src = "use gum.defaults.Message\n\ncontract V:\n    u256 got\n\n    export payable fn receive():\n        V.got = V.got + Message.value()\n\n    export fn poke():\n        V.got = 0\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);

    assert!(
        output.contains("/* poke */ {\n          if callvalue() { revert(0, 0) }"),
        "poke() must carry its own nonpayable guard, got:\n{}",
        output
    );

    assert!(
        !output.contains(
            "let selector := shr(224, calldataload(0))\n      if callvalue() { revert(0, 0) }"
        ),
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
    assert_compile_fails(
        "contract App:\n    export payable fn receive(u256 x):\n        var y = x\n",
    );
}

#[test]
fn receive_returns_nothing() {
    assert_compile_fails(
        "contract App:\n    export payable fn receive() -> u256:\n        return 1\n",
    );
}

#[test]
fn receive_must_be_exported() {
    assert_compile_fails("contract C:\n    u256 x\n\n    payable fn receive():\n        C.x = 1\n");
}

#[test]
fn receive_is_only_reserved_inside_a_contract() {
    assert_compiles(
        "fn receive() -> u256:\n    return 1\n\ncontract C:\n    export fn f() -> u256:\n        return receive()\n",
    );
}

#[test]
fn fallback_may_be_nonpayable() {
    assert_compiles("contract V:\n    u256 got\n\n    export fn fallback():\n        V.got = 1\n");
}

#[test]
fn new_on_a_contract_emits_create_not_an_allocation() {
    let src = "contract Child:\n    u256 v\n\n    fn new(u256 x):\n        self.v = x\n\ncontract Factory:\n    u256 n\n\n    export fn make(u256 x) -> Account:\n        return Child.new(x)\n";
    assert_output_contains(src, "function __deploy_Child(a0) -> addr {");
    assert_output_contains(src, "datacopy(ptr, dataoffset(\"Child\"), size)");

    assert_output_contains(src, "let alen := 32");
    assert_output_contains(src, "let blob := add(ptr, size)");
    assert_output_contains(src, "mstore(add(blob, 0), a0)");
    assert_output_contains(src, "addr := create(0, ptr, add(size, alen))");

    assert_output_contains(src, "if iszero(addr) { gum_bubble_revert() }");
}

#[test]
fn a_deployed_child_is_nested_inside_its_deployer_s_runtime() {
    let src = "contract Child:\n    u256 v\n\ncontract Factory:\n    u256 n\n\n    export fn make() -> Account:\n        return Child.new()\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    let factory = output
        .split("object \"Factory\" {")
        .nth(1)
        .expect("no Factory object");
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
    let src = "contract Child:\n    u256 v\n\ncontract Factory:\n    u256 n\n\n    export fn make() -> Account:\n        return Child.new()\n";
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
    let src = "contract A:\n    u256 x\n    u256 y\n\n    export fn set():\n        A.x = 1\n        A.y = 2\n\ncontract B:\n    u256 p\n\n    export fn set():\n        B.p = 3\n";
    let (ok, output) = run_gumc(src);
    assert!(ok, "expected success, got:\n{}", output);
    let b = output
        .split("--- Contract: B ---")
        .nth(1)
        .expect("no B section");
    assert!(
        b.contains("sstore(0, 3)"),
        "B.p must be slot 0, not stacked after A's fields:\n{}",
        b
    );
}

#[test]
fn deploying_a_contract_with_a_string_constructor_arg_encodes_head_and_tail() {
    let src = "contract Child:\n    String name\n    u256 n\n\n    fn new(String s, u256 v):\n        self.name = s\n        self.n = v\n\ncontract Factory:\n    u256 c\n\n    export fn make(String s, u256 v) -> Account:\n        return Child.new(s, v)\n";

    assert_output_contains(src, "let tail := 64");
    assert_output_contains(src, "mstore(add(blob, 0), tail)");
    assert_output_contains(src, "mstore(add(blob, tail), a0_len)");

    assert_output_contains(src, "mstore(add(blob, 32), a1)");

    assert_output_contains(src, "alen := add(alen, add(32, a0_pad))");
    assert_assembles(src);
}

#[test]
fn deploying_a_contract_with_an_array_constructor_arg_encodes_it() {
    let src = "contract Child:\n    u256 n\n\n    fn new([u256] xs):\n        self.n = xs.length\n\ncontract Factory:\n    u256 c\n\n    export fn make([u256] xs) -> Account:\n        return Child.new(xs)\n";
    assert_output_contains(src, "let a0_abi := gum_abi_arr_u256_size(a0)");
    assert_output_contains(
        src,
        "tail := add(tail, gum_abi_arr_u256_put(add(blob, tail), a0))",
    );
    assert_output_contains(src, "gum_abi_arr_u256_mem(args_mem,");
    assert_assembles(src);
}

#[test]
fn deploying_a_contract_with_a_fixed_array_constructor_arg_encodes_it_inline() {
    let src = "contract Child:\n    u256 n\n\n    fn new([u8; 3] xs, u256 v):\n        self.n = v\n\ncontract Factory:\n    u256 c\n\n    export fn make([u8; 3] xs, u256 v) -> Account:\n        return Child.new(xs, v)\n";
    assert_output_contains(src, "pop(gum_abi_farr3_u8_put(add(blob, 0), a0))");
    assert_output_contains(src, "gum_abi_farr_put(dst, ptr, 3, 1)");

    assert_output_contains(src, "mstore(add(blob, 96), a1)");
    assert_output_contains(
        src,
        "let param_xs := gum_abi_farr3_u8_mem(args_mem, 0, _args_len)",
    );
    assert_assembles(src);
}

#[test]
fn a_deployment_cycle_is_a_compile_error() {
    assert_compile_fails(
        "contract A:\n    u256 x\n\n    export fn make() -> Account:\n        return B.new()\n\ncontract B:\n    u256 y\n\n    export fn make() -> Account:\n        return A.new()\n",
    );
}

#[test]
fn new_on_a_plain_class_still_allocates_memory() {
    let src = "class Point:\n    u256 x\n\n    fn new(self, u256 v):\n        self.x = v\n\ncontract C:\n    export fn go() -> u256:\n        var p = Point.new(7)\n        return p.x\n";
    assert_output_contains(src, "allocate_memory");
    let (_, output) = run_gumc(src);
    assert!(
        !output.contains("__deploy_Point"),
        "a plain class must not be deployed"
    );
}

#[test]
fn the_old_new_keyword_syntax_is_retired() {
    let (ok, _) = run_gumc(
        "class Point:\n    u256 x\n\n    fn new(u256 v):\n        self.x = v\n\ncontract C:\n    export fn go() -> u256:\n        var p = new Point(7)\n        return p.x\n",
    );
    assert!(!ok, "the old `new Point(...)` syntax must no longer parse");
}

#[test]
fn construction_uses_the_type_dot_new_form() {
    let src = "class Point:\n    u256 x\n\n    fn new(self, u256 v):\n        self.x = v\n\ncontract C:\n    export fn go() -> u256:\n        var p = Point.new(7)\n        return p.x\n";
    assert_compiles(src);
}

#[test]
fn free_functions_are_callable() {
    let src = "fn triple(u256 x) -> u256:\n    return x + x + x\n\ncontract C:\n    export fn go(u256 n) -> u256:\n        return triple(n)\n";
    assert_compiles(src);
}

#[test]
fn numeric_type_bounds_are_static_calls() {
    let src = "contract C:\n    export fn a() -> u256:\n        return u256.max()\n    export fn b() -> u256:\n        return u256.min()\n    export fn c() -> u8:\n        return u8.max()\n    export fn d() -> i256:\n        return i8.min()\n";
    assert_compiles(src);
}

#[test]
fn a_function_without_self_is_an_associated_function() {
    let src = "class Box:\n    u256 v\n\n    fn new(self, u256 x):\n        self.v = x\n\n    fn twice(u256 a) -> u256:\n        return a + a\n\ncontract C:\n    export fn go() -> u256:\n        return Box.twice(21)\n";
    assert_output_contains(src, "function Box_twice(a) -> ret");
    assert_output_contains(src, "ret := Box_twice(21)");
}

#[test]
fn an_associated_function_can_be_called_on_an_instance_without_a_receiver() {
    let src = "class Box:\n    u256 v\n\n    fn new(self, u256 x):\n        self.v = x\n\n    fn twice(u256 a) -> u256:\n        return a + a\n\ncontract C:\n    export fn go() -> u256:\n        var b = Box.new(1)\n        return b.twice(9)\n";
    assert_output_contains(src, "ret := Box_twice(9)");
}

#[test]
fn a_method_call_with_the_wrong_arg_count_is_rejected() {
    let inst = "class P:\n    u256 x\n\n    fn new(self, u256 v):\n        self.x = v\n\n    fn sum(self) -> u256:\n        return self.x\n\ncontract C:\n    export fn f() -> u256:\n        var p = P.new(1)\n        return p.sum(99)\n";
    let (ok, out) = run_gumc(inst);
    assert!(!ok, "instance-method arity mismatch must be rejected:\n{}", out);
    assert!(out.contains("takes 0 argument"), "unclear arity error:\n{}", out);

    let assoc = "class P:\n    u256 x\n\n    fn new(self, u256 v):\n        self.x = v\n\n    fn tag() -> u256:\n        return 7\n\ncontract C:\n    export fn f() -> u256:\n        return P.tag(99)\n";
    let (ok2, out2) = run_gumc(assoc);
    assert!(!ok2, "associated-fn arity mismatch must be rejected:\n{}", out2);
    assert!(out2.contains("takes 0 argument"), "unclear arity error:\n{}", out2);
}

#[test]
fn a_one_arg_class_cast_is_transparent() {
    let src = "use gum.defaults.Account\n\ncontract C:\n    export fn f(Account a) -> bool:\n        return a == Account(0)\n";
    assert_output_contains(src, "eq(a, 0)");
    let (ok, out) = run_gumc(src);
    assert!(ok, "Account(0) cast must compile, got:\n{}", out);
    assert!(!out.contains("Account(0)"), "cast leaked into Yul:\n{}", out);
}

#[test]
fn a_generic_associated_function_is_callable_on_the_type() {
    let src = "class Box(All <T>):\n    T value\n\n    fn new(self, T v):\n        self.value = v\n\n    fn tag() -> u256:\n        return 7\n\ncontract C:\n    export fn f() -> u256:\n        return Box(u256).tag()\n";
    assert_output_contains(src, "Box_u256_tag()");
}

#[test]
fn a_function_that_declares_self_takes_a_receiver() {
    let src = "class Box:\n    u256 v\n\n    fn new(self, u256 x):\n        self.v = x\n\n    fn get(self) -> u256:\n        return self.v\n\ncontract C:\n    export fn go() -> u256:\n        var b = Box.new(7)\n        return b.get()\n";
    assert_output_contains(src, "function Box_get(self) -> ret");
}

#[test]
fn an_array_argument_decodes_from_its_abi_offset_not_as_a_scalar() {
    let src = "contract C:\n    export fn sum([u256] xs) -> u256:\n        mut u256 s = 0\n        for x in xs:\n            s = s + x\n        return s\n";
    assert_output_contains(src, "gum_abi_arr_u256_cd(add(4, calldataload(4)))");
    assert_output_contains(src, "ptr := gum_abi_arr_cd(off, 32)");
    assert_output_contains(src, "\"type\": \"uint256[]\"");
    assert_assembles(src);
}

#[test]
fn a_narrow_array_converts_between_wire_and_memory_widths() {
    let src = "contract C:\n    export fn echo([u8] xs) -> [u8]:\n        return xs\n";
    assert_output_contains(
        src,
        "let param_xs := gum_abi_arr_u8_cd(add(4, calldataload(4)))",
    );
    assert_output_contains(src, "ptr := gum_abi_arr_cd(off, 1)");
    assert_output_contains(src, "let _w := gum_abi_arr_u8_put(add(_out, 32), _p)");
    assert_output_contains(src, "written := gum_abi_arr_put(dst, ptr, 1)");
    assert_assembles(src);
}

#[test]
fn indexing_a_memory_array_is_bounds_checked() {
    let src = "contract C:\n    export fn at([u256] xs, u256 i) -> u256:\n        return xs[i]\n";
    assert_output_contains(src, "function gum_marr_addr(ptr, i, esz) -> a {");
    assert_output_contains(src, "if iszero(lt(i, div(mload(ptr), esz)))");
    assert_assembles(src);
}

#[test]
fn writing_a_memory_array_element_is_bounds_checked_once() {
    let src = "contract C:\n    export fn set([u8] xs, u256 i, u8 v) -> u256:\n        xs[i] = v\n        return xs.length\n";
    assert_output_contains_numbered(src, "let __ma_", " := gum_marr_addr");
    assert_assembles(src);
}

#[test]
fn memory_array_length_is_an_element_count_not_a_byte_count() {
    assert_output_contains(
        "contract C:\n    export fn n([u256] xs) -> u256:\n        return xs.length\n",
        "div(mload(xs), 32)",
    );
}

#[test]
fn an_array_of_arrays_crosses_the_abi() {
    let src = "contract C:\n    export fn f([[u256]] xs) -> u256:\n        return xs[0][1]\n";
    assert_output_contains(src, "\"type\": \"uint256[][]\"");
    assert_output_contains(
        src,
        "let param_xs := gum_abi_arr_arr_u256_cd(add(4, calldataload(4)))",
    );

    assert_output_contains(
        src,
        "mstore(add(add(ptr, 32), mul(i, 32)), gum_abi_arr_u256_cd(add(base, eo)))",
    );
    assert_assembles(src);
}

#[test]
fn a_dynamic_value_inside_a_storage_aggregate_is_rejected() {
    assert_compile_fails(
        "contract C:\n    [[u256]] g\n\n    export fn f(u256 i, u256 j) -> u256:\n        return C.g[i][j]\n",
    );
    assert_compile_fails(
        "contract C:\n    [[u256]; 2] g\n\n    export fn f() -> u256:\n        return 1\n",
    );
    assert_compile_fails(
        "use gum.defaults.String\n\ncontract C:\n    [String] g\n\n    export fn f() -> u256:\n        return 1\n",
    );

    assert_compile_fails(
        "use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, [[u256]]) m\n\n    export fn f() -> u256:\n        return 1\n",
    );

    assert_compiles(
        "contract C:\n    export fn f([[u256]] xs) -> u256:\n        return xs[0][0]\n",
    );

    assert_compiles(
        "use gum.defaults.String\n\ncontract C:\n    String s\n\n    export fn f() -> u256:\n        return C.s.length\n",
    );
    assert_compiles(
        "class P:\n    u256 x\n\ncontract C:\n    [P] xs\n\n    export fn f(u256 i) -> u256:\n        return C.xs[i].x\n",
    );
    assert_compiles(
        "use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, HashMap(Account, u256)) m\n\n    export fn f(Account a, Account b) -> u256:\n        return C.m[a][b]\n",
    );
    assert_compiles(
        "contract C:\n    [u256] xs\n\n    export fn f(u256 i) -> u256:\n        return C.xs[i]\n",
    );

    assert_compiles(
        "use gum.defaults.Account\nuse gum.defaults.String\n\ncontract C:\n    HashMap(Account, String) m\n\n    export fn f(Account a, String s):\n        C.m[a] = s\n\n    export fn g(Account a) -> String:\n        return C.m[a]\n",
    );

    assert_compiles(
        "use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, [u256]) m\n\n    export fn f(Account a, u256 v):\n        C.m[a].push(v)\n\n    export fn g(Account a, u256 i) -> u256:\n        return C.m[a][i]\n\n    export fn n(Account a) -> u256:\n        return C.m[a].length\n",
    );
}

#[test]
fn a_string_array_across_the_abi_is_accepted() {
    assert_compiles(
        "use gum.defaults.String\n\ncontract C:\n    export fn f([String] xs) -> [String]:\n        return xs\n",
    );
    assert_compiles(
        "use gum.defaults.String\n\ncontract C:\n    export fn f([String] xs) -> String:\n        return xs[0]\n",
    );
    assert_compiles(
        "use gum.defaults.String\n\ncontract C:\n    export fn f([[String]] xs) -> u256:\n        return xs.length\n",
    );
}

#[test]
fn a_dynamic_struct_crosses_the_abi_but_not_nested_or_in_an_array() {
    assert_compiles(
        "use gum.defaults.String\n\nclass Meta:\n    u256 id\n    String name\n\n    fn new(self, u256 i, String n):\n        self.id = i\n        self.name = n\n\ncontract C:\n    export fn echo(Meta m) -> Meta:\n        return m\n",
    );
    assert_compiles(
        "class Nums:\n    u256 id\n    [u256] xs\n\ncontract C:\n    export fn f(Nums n) -> u256:\n        return n.id\n",
    );

    assert_compile_fails(
        "use gum.defaults.String\n\nclass Meta:\n    u256 id\n    String name\n\ncontract C:\n    export fn f([Meta] ms) -> u256:\n        return 1\n",
    );
}

#[test]
fn interface_args_are_encoded_head_tail() {
    let src = "contract C:\n    export fn f(Account t, String s) -> u256:\n        return ISink(t).take(s)\n\ninterface ISink:\n    fn take(String s) -> u256\n";
    assert_output_contains(src, "let a0_len := gum_str_len(a0)");
    assert_output_contains(src, "mstore(add(blob, tail), a0_len)");
}

#[test]
fn an_interface_returning_a_non_scalar_decodes_it() {
    let src = "use gum.defaults.String\n\ninterface I:\n    fn name() -> String\n\ncontract C:\n    export fn f(Account t) -> u256:\n        var s = I(t).name()\n        return s.length\n";
    assert_output_contains(src, "gum_abi_str_mem(rd, mload(rd), returndatasize())");
    let arr = "interface I:\n    fn xs() -> [u256]\n\ncontract C:\n    export fn f(Account t) -> u256:\n        var a = I(t).xs()\n        return a.length\n";
    assert_output_contains(arr, "gum_abi_arr_u256_mem(rd, mload(rd), returndatasize())");
}

#[test]
fn a_dynamic_array_of_structs_across_the_abi_is_accepted() {
    assert_compiles(
        "class P:\n    u128 a\n    u256 b\n\ncontract C:\n    export fn f([P] xs) -> [P]:\n        return xs\n",
    );
}

#[test]
fn an_array_of_non_static_structs_across_the_abi_is_rejected() {
    assert_compile_fails(
        "class W:\n    u256 z\n    String s\n\ncontract C:\n    export fn f([W] xs) -> u256:\n        return 1\n",
    );
}

#[test]
fn a_fixed_array_of_structs_rides_inline_in_the_head() {
    let src = "class P:\n    u256 x\n\ncontract C:\n    export fn f([P; 2] xs, u256 v) -> u256:\n        return xs[1].x + v\n";
    assert_output_contains(src, "\"type\": \"tuple[2]\"");
    assert_output_contains(src, "let param_xs := gum_abi_farr2_P_cd(4)");

    assert_output_contains(src, "let param_v := calldataload(68)");
    assert_assembles(src);
}

#[test]
fn a_revert_counts_as_diverging_for_the_return_check() {
    assert_compiles(
        "enum Error:\n    Bad(u256 x)\n\ncontract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            return x\n        revert Error.Bad(x)\n",
    );

    assert_compiles(
        "enum Error:\n    B(u256 x)\n\ncontract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            return 1\n        else:\n            revert Error.B(x)\n",
    );

    assert_compile_fails(
        "contract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            return x\n",
    );
    assert_compile_fails(
        "enum Error:\n    B(u256 x)\n\ncontract C:\n    export fn f(u256 x) -> u256:\n        if x > 0:\n            revert Error.B(x)\n",
    );
}

#[test]
fn a_payload_free_enum_is_one_byte_like_solidity() {
    let src = "enum S:\n    A\n    B\n\ncontract C:\n    S state\n\n    export fn set(S s):\n        C.state = s\n";

    assert_output_contains(
        src,
        "sstore(0, or(and(sload(0), not(shl(0, 0xff))), shl(0, and(s, 0xff))))",
    );
}

#[test]
fn a_payload_enum_has_no_storage_layout() {
    let head = "enum R:\n    Ok(u256 x)\n    Err\n\n";
    for body in [
        "contract C:\n    R r\n\n    export fn f() -> u256:\n        return 1\n",
        "use gum.defaults.Account\n\ncontract C:\n    HashMap(Account, R) m\n\n    export fn f() -> u256:\n        return 1\n",
        "contract C:\n    [R] xs\n\n    export fn f() -> u256:\n        return 1\n",
        "class S:\n    R r\n\ncontract C:\n    export fn f() -> u256:\n        return 1\n",
    ] {
        let (ok, out) = run_gumc(&format!("{}{}", head, body));
        assert!(!ok, "a payload enum must not be given a layout:\n{}", out);
        assert!(
            out.contains("has no storage layout"),
            "expected a layout error, got:\n{}",
            out
        );
    }
}

#[test]
fn an_enum_param_decodes_from_one_word() {
    let src = "enum S:\n    A\n    B\n\ncontract C:\n    export fn f(S s, u256 x) -> u256:\n        return x\n";
    assert_output_contains(src, "let param_s_raw := calldataload(4)");

    assert_output_contains(src, "if iszero(lt(param_s_raw, 2)) { revert(0, 0) }");
    assert_output_contains(src, "let param_s := and(param_s_raw, 0xff)");
    assert_output_contains(src, "let param_x := calldataload(36)");
}

#[test]
fn an_enum_with_a_payload_across_the_abi_is_rejected() {
    assert_compile_fails(
        "enum E:\n    Bare\n    Carrying(u256)\n\ncontract C:\n    export fn f(E e) -> u256:\n        return 1\n",
    );

    assert_compiles(
        "enum S:\n    A\n    B\n\ncontract C:\n    export fn f(S s) -> S:\n        return s\n",
    );
}

#[test]
fn a_struct_across_the_abi_is_accepted() {
    assert_compiles(
        "use gum.defaults.Account

class P:
    u128 a
    u256 b
    Account c

contract C:
    export fn f(P p) -> P:
        return p
",
    );
}

#[test]
fn a_struct_nesting_another_struct_across_the_abi_is_rejected() {
    assert_compile_fails(
        "class I:
    u256 x

class O:
    u256 y
    I n

contract C:
    export fn f(O o) -> u256:
        return o.y
",
    );
}

#[test]
fn a_struct_param_decodes_through_its_own_codec() {
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
    assert_compiles(
        "use gum.defaults.Account\n\ncontract C:\n    export fn f([Account] a, [bool] b, [i8] c, [u256; 2] d) -> [i8]:\n        return c\n",
    );
}

#[test]
fn a_transient_scalar_uses_tstore_and_tload() {
    let src = "contract C:\n    transient u256 t\n\n    export fn set():\n        C.t = 1\n\n    export fn get() -> u256:\n        return C.t\n";
    assert_output_contains(src, "tstore(0, 1)");
    assert_output_contains(src, "tload(0)");
    assert_assembles(src);
}

#[test]
fn a_transient_field_and_a_persistent_one_may_share_a_slot_number() {
    let src = "contract C:\n    u256 a\n    transient u256 t\n\n    export fn go():\n        C.a = 1\n        C.t = 2\n";
    assert_output_contains(src, "sstore(0, 1)");
    assert_output_contains(src, "tstore(0, 2)");
    assert_assembles(src);
}

#[test]
fn transient_collections_get_their_own_helper_family() {
    let src = "use gum.defaults.Account\n\ncontract C:\n    transient [u256] a\n    transient HashMap(Account, u256) m\n    transient String s\n\n    export fn go(Account w, String v):\n        C.a.push(1)\n        C.m[w] = 2\n        C.s = v\n";
    assert_output_contains(src, "function dpk_push_t(");
    assert_output_contains(src, "let n := tload(len_slot)");
    assert_output_contains(src, "function gum_sstr_store_t(");
    assert_output_contains(src, "tstore(gum_hash_slot(");
    assert_assembles(src);
}

#[test]
fn a_persistent_only_contract_emits_no_transient_helper() {
    let src = "contract C:\n    [u256] a\n\n    export fn go():\n        C.a.push(1)\n";
    assert_output_contains(src, "function dpk_push(");
    let (_, out) = run_gumc(src);
    assert!(
        !out.contains("dpk_push_t"),
        "no transient helper should be emitted:\n{}",
        out
    );
    assert!(
        !out.contains("tstore"),
        "no transient opcode should be emitted:\n{}",
        out
    );
}

#[test]
fn transient_fields_are_absent_from_the_storage_lock() {
    let dir = std::env::temp_dir().join(format!("gum_tlock_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lock = dir.join("layout.json");

    let (ok, out) = run_gumc_with_args(
        "contract C:\n    u256 kept\n    transient u256 scratch\n\n    export fn go():\n        C.kept = 1\n        C.scratch = 2\n",
        &["--lock", &lock.to_string_lossy()],
    );
    assert!(ok, "expected success, got:\n{}", out);
    let manifest = std::fs::read_to_string(&lock).unwrap();
    assert!(
        manifest.contains("kept"),
        "the persistent field must be committed:\n{}",
        manifest
    );
    assert!(
        !manifest.contains("scratch"),
        "a transient field must not be committed:\n{}",
        manifest
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transient_on_a_plain_class_field_is_rejected() {
    assert_compile_fails(
        "class P:\n    transient u256 x\n\ncontract C:\n    export fn go() -> u256:\n        var p = P.new()\n        return p.x\n",
    );
}

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
        if c == '{' {
            depth += 1;
        }
        if c == '}' {
            depth -= 1;
            if depth == 0 {
                return rest[open..open + i + 1].to_string();
            }
        }
    }
    rest.to_string()
}

#[test]
fn only_functions_that_call_out_carry_a_reentrancy_guard() {
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
    assert!(
        ok,
        "compile failed:
{}",
        out
    );

    let touch = case_body(&out, "touch");
    assert!(
        !touch.contains("tstore"),
        "touch should not be guarded:
{}",
        touch
    );

    let direct = case_body(&out, "direct");
    assert!(
        direct.contains("tstore"),
        "direct must be guarded:
{}",
        direct
    );

    let indirect = case_body(&out, "indirect");
    assert!(
        indirect.contains("tstore"),
        "indirect must be guarded through its helper:
{}",
        indirect
    );
}
