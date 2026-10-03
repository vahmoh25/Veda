//! Prints the prime numbers below a limit with the sieve of Eratosthenes.

use std::env;

/// Default upper limit when none is given on the command line.
const LIMIT: usize = 100;

/// Returns every prime number smaller than `limit`.
fn primes_below(limit: usize) -> Vec<usize> {
    let mut composite = vec![false; limit];
    let mut primes = Vec::new();
    for n in 2..limit {
        if composite[n] {
            continue;
        }
        primes.push(n);
        // Every multiple of a prime is composite.
        for multiple in (n * n..limit).step_by(n) {
            composite[multiple] = true;
        }
    }
    primes
}

fn main() {
    let limit = env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(LIMIT);
    let primes = primes_below(limit);
    println!("{} primes below {limit}:", primes.len());
    for chunk in primes.chunks(10) {
        let line: Vec<String> = chunk.iter().map(|p| format!("{p:>5}")).collect();
        println!("{}", line.join(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_primes() {
        assert_eq!(primes_below(20), [2, 3, 5, 7, 11, 13, 17, 19]);
    }
}
