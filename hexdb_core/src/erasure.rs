// HexDB Core Erasure Coding for small shards
//
// A systematic Reed-Solomon code over GF(256) for the hex's 4 data + 2 parity
// shards: parity row i is a Cauchy row (1 / (x_i + y_j)), so any four of the
// six shards determine the data. It's table-driven, with no per-call setup,
// which makes it much faster than reed-solomon-simd for the small shards of
// typical documents (that library's decoding has a fixed cost of about a
// tenth of a millisecond). Large shards still use reed-solomon-simd; the hex
// picks by shard length, so a document's shards are always encoded and
// repaired with the same code.

const DATA: usize = 4;
const TOTAL: usize = 6;

/// GF(256) with the polynomial x^8 + x^4 + x^3 + x^2 + 1 (0x11d).
struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    /// mul[a][b] = a * b
    mul: Box<[[u8; 256]; 256]>,
}

fn tables() -> &'static Tables {
    static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, e) in exp.iter_mut().take(255).enumerate() {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        let mut mul = Box::new([[0u8; 256]; 256]);
        for a in 1..256 {
            for b in 1..256 {
                mul[a][b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        Tables { exp, log, mul }
    })
}

fn inverse(a: u8) -> u8 {
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// Row `r` of the 6x4 generator matrix: identity for data rows, Cauchy for parity.
fn generator_row(r: usize) -> [u8; DATA] {
    let mut row = [0u8; DATA];
    if r < DATA {
        row[r] = 1;
    } else {
        // x = r (4 or 5), y = j (0..3): x ^ y is never 0.
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = inverse((r as u8) ^ (j as u8));
        }
    }
    row
}

/// out ^= c * input, byte by byte.
fn mul_add(out: &mut [u8], input: &[u8], c: u8) {
    match c {
        0 => {}
        1 => out.iter_mut().zip(input).for_each(|(o, i)| *o ^= i),
        c => {
            let row = &tables().mul[c as usize];
            out.iter_mut().zip(input).for_each(|(o, i)| *o ^= row[*i as usize]);
        }
    }
}

/// The two parity shards of four equal-length data shards.
pub(crate) fn encode<S: AsRef<[u8]>>(data: &[S]) -> [Vec<u8>; 2] {
    let len = data[0].as_ref().len();
    let mut parity = [vec![0u8; len], vec![0u8; len]];
    for (p, out) in parity.iter_mut().enumerate() {
        let row = generator_row(DATA + p);
        for (j, shard) in data.iter().enumerate() {
            mul_add(out, shard.as_ref(), row[j]);
        }
    }
    parity
}

/// Invert a 4x4 matrix over GF(256); `None` if it's singular.
fn invert(mut m: [[u8; DATA]; DATA]) -> Option<[[u8; DATA]; DATA]> {
    let t = tables();
    let mut inv = [[0u8; DATA]; DATA];
    for (i, row) in inv.iter_mut().enumerate() {
        row[i] = 1;
    }
    for col in 0..DATA {
        let pivot = (col..DATA).find(|&r| m[r][col] != 0)?;
        m.swap(col, pivot);
        inv.swap(col, pivot);
        let scale = inverse(m[col][col]);
        for k in 0..DATA {
            m[col][k] = t.mul[m[col][k] as usize][scale as usize];
            inv[col][k] = t.mul[inv[col][k] as usize][scale as usize];
        }
        for r in 0..DATA {
            if r != col && m[r][col] != 0 {
                let f = m[r][col];
                for k in 0..DATA {
                    m[r][k] ^= t.mul[f as usize][m[col][k] as usize];
                    inv[r][k] ^= t.mul[f as usize][inv[col][k] as usize];
                }
            }
        }
    }
    Some(inv)
}

/// Fill in missing shards (`None`) from any four present ones, all of equal length.
pub(crate) fn reconstruct(shards: &mut [Option<Vec<u8>>]) -> Result<(), ()> {
    let present: Vec<usize> = (0..TOTAL).filter(|&i| shards[i].is_some()).collect();
    if present.len() < DATA {
        return Err(());
    }
    if present.len() == TOTAL {
        return Ok(());
    }
    let len = shards[present[0]].as_ref().map(Vec::len).unwrap_or(0);
    if (0..DATA).any(|i| shards[i].is_none()) {
        // data = inverse(rows of the shards we have) * those shards
        let rows: Vec<usize> = present.iter().copied().take(DATA).collect();
        let mut m = [[0u8; DATA]; DATA];
        for (k, &r) in rows.iter().enumerate() {
            m[k] = generator_row(r);
        }
        let inv = invert(m).ok_or(())?;
        let mut restored = Vec::new();
        for i in (0..DATA).filter(|&i| shards[i].is_none()) {
            let mut out = vec![0u8; len];
            for (k, &r) in rows.iter().enumerate() {
                mul_add(&mut out, shards[r].as_deref().ok_or(())?, inv[i][k]);
            }
            restored.push((i, out));
        }
        for (i, shard) in restored {
            shards[i] = Some(shard);
        }
    }
    if shards[DATA..].iter().any(Option::is_none) {
        let data: Vec<&[u8]> = shards[..DATA].iter().map(|s| s.as_deref().unwrap_or(&[])).collect();
        let parity = encode(&data);
        for (p, shard) in parity.into_iter().enumerate() {
            if shards[DATA + p].is_none() {
                shards[DATA + p] = Some(shard);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize, seed: u8) -> Vec<Vec<u8>> {
        (0..DATA).map(|j| (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed).wrapping_add(j as u8 * 17)).collect()).collect()
    }

    #[test]
    fn any_two_lost_shards_are_rebuilt() {
        for len in [0usize, 1, 2, 7, 64, 1000] {
            let data = sample(len, len as u8);
            let parity = encode(&data);
            let all: Vec<Vec<u8>> = data.iter().cloned().chain(parity.iter().cloned()).collect();
            for a in 0..TOTAL {
                for b in a..TOTAL {
                    let mut shards: Vec<Option<Vec<u8>>> = all.iter().cloned().map(Some).collect();
                    shards[a] = None;
                    shards[b] = None;
                    reconstruct(&mut shards).unwrap();
                    let back: Vec<Vec<u8>> = shards.into_iter().map(|s| s.unwrap()).collect();
                    assert_eq!(back, all, "len {} lost {} and {}", len, a, b);
                }
            }
            let mut three: Vec<Option<Vec<u8>>> = all.iter().cloned().map(Some).collect();
            three[0] = None;
            three[2] = None;
            three[5] = None;
            assert!(reconstruct(&mut three).is_err());
        }
    }

    #[test]
    fn field_arithmetic_is_consistent() {
        let t = tables();
        for a in 1..=255u8 {
            assert_eq!(t.mul[a as usize][inverse(a) as usize], 1);
        }
    }
}
