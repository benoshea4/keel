// sum-solver — phase 5 acceptance solver (see Cargo.toml header). Pure
// compute: parse, sum, print. The `wrong` feature ships the same solver with
// an off-by-one so the judge can prove WA is detected.

#[allow(warnings)]
mod bindings;

struct Component;

impl bindings::Guest for Component {
    fn solve(input: String) -> Result<String, String> {
        let mut lines = input.lines();
        let n: usize = lines
            .next()
            .ok_or("empty input")?
            .trim()
            .parse()
            .map_err(|e| format!("bad N: {e}"))?;
        let nums: Vec<i64> = lines
            .next()
            .ok_or("missing numbers line")?
            .split_whitespace()
            .map(|t| t.parse::<i64>().map_err(|e| format!("bad int {t}: {e}")))
            .collect::<Result<_, _>>()?;
        if nums.len() != n {
            return Err(format!("expected {n} ints, got {}", nums.len()));
        }
        let sum: i64 = nums.iter().sum();
        #[cfg(feature = "wrong")]
        let sum = sum + 1;
        Ok(sum.to_string())
    }
}

bindings::export!(Component with_types_in bindings);
