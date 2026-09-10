use deepseek_agent::client::pow::PowSolver;

#[test]
fn test_pow_known_answer() {
    let mut s = PowSolver::from_file("wasm/sha3.wasm").expect("load wasm");
    let ans = s
        .solve_challenge(
            "430f64a069c8b8204f7fd1fd79d5b10584b26fc55615616e3170b9f627d7eaf8",
            "b05cab0e3998e06ca822",
            1789025147648,
            144000,
        )
        .expect("solve");
    assert_eq!(ans, 22195.0, "PoW answer mismatch");
}
