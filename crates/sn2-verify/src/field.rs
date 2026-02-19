use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::ToPrimitive;

#[allow(dead_code)]
pub fn to_field_repr(value: &BigUint, modulus: &BigUint) -> BigUint {
    value.mod_floor(modulus)
}

pub fn from_field_repr(value: &BigUint, modulus: &BigUint) -> BigInt {
    let half = modulus >> 1;
    if *value > half {
        let diff = modulus - value;
        BigInt::from_biguint(Sign::Minus, diff)
    } else {
        BigInt::from_biguint(Sign::Plus, value.clone())
    }
}

pub fn scale_to_field(
    values: &[f64],
    scale_base: u64,
    scale_exp: u64,
    modulus: &BigUint,
) -> Vec<BigUint> {
    if scale_base == 0 || scale_exp == 0 {
        return values
            .iter()
            .map(|v| {
                let iv = *v as i128;
                if iv < 0 {
                    let abs_val = BigUint::from((-iv) as u128);
                    if abs_val > *modulus {
                        modulus - (abs_val.mod_floor(modulus))
                    } else {
                        modulus - &abs_val
                    }
                } else {
                    BigUint::from(iv as u128).mod_floor(modulus)
                }
            })
            .collect();
    }
    let scale_f64 = (scale_base as f64).powi(scale_exp as i32);
    values
        .iter()
        .map(|v| {
            let scaled = (*v * scale_f64).round() as i128;
            if scaled < 0 {
                let abs_val = BigUint::from((-scaled) as u128);
                modulus - abs_val.mod_floor(modulus)
            } else {
                BigUint::from(scaled as u128).mod_floor(modulus)
            }
        })
        .collect()
}

pub fn descale_outputs(outputs: &[BigInt], scale_base: u64, scale_exp: u64) -> Vec<f64> {
    if scale_base == 0 || scale_exp == 0 {
        return outputs.iter().map(|v| v.to_f64().unwrap_or(0.0)).collect();
    }
    let scale = (scale_base as f64).powi(scale_exp as i32);
    outputs
        .iter()
        .map(|v| v.to_f64().unwrap_or(0.0) / scale)
        .collect()
}

pub fn compare_field_values(
    expected: &[BigUint],
    actual: &[BigUint],
    modulus: &BigUint,
    tolerance: u64,
) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let tol = BigUint::from(tolerance);
    let neg_tol = modulus - &tol;
    for (e, a) in expected.iter().zip(actual.iter()) {
        let diff = if e >= a {
            (e - a).mod_floor(modulus)
        } else {
            modulus - (a - e).mod_floor(modulus)
        };
        if diff > tol && diff < neg_tol {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_traits::{One, Zero};

    #[test]
    fn test_from_field_repr_positive() {
        let modulus = BigUint::from(100u64);
        let val = BigUint::from(30u64);
        assert_eq!(from_field_repr(&val, &modulus), BigInt::from(30));
    }

    #[test]
    fn test_from_field_repr_negative() {
        let modulus = BigUint::from(100u64);
        let val = BigUint::from(80u64);
        assert_eq!(from_field_repr(&val, &modulus), BigInt::from(-20));
    }

    #[test]
    fn test_from_field_repr_large_modulus() {
        let modulus = BigUint::parse_bytes(
            b"30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001",
            16,
        )
        .unwrap();
        let val = &modulus - BigUint::from(42u64);
        let result = from_field_repr(&val, &modulus);
        assert_eq!(result, BigInt::from(-42));
    }

    #[test]
    fn test_scale_to_field() {
        let modulus = BigUint::from(1000003u64);
        let vals = vec![0.5, -0.25, 1.0];
        let result = scale_to_field(&vals, 2, 18, &modulus);
        let scale = 2u64.pow(18) as f64;
        assert_eq!(result[0], BigUint::from((0.5 * scale).round() as u64));
        let neg_scaled = (-0.25 * scale).round() as i128;
        let expected_neg = &modulus - BigUint::from((-neg_scaled) as u128);
        assert_eq!(result[1], expected_neg);
    }

    #[test]
    fn test_compare_field_values_exact() {
        let modulus = BigUint::from(1000003u64);
        let a = vec![BigUint::from(100u64), BigUint::from(200u64)];
        assert!(compare_field_values(&a, &a, &modulus, 1));
    }

    #[test]
    fn test_compare_field_values_tolerance() {
        let modulus = BigUint::from(1000003u64);
        let a = vec![BigUint::from(100u64)];
        let b = vec![BigUint::from(101u64)];
        assert!(compare_field_values(&a, &b, &modulus, 1));
    }

    #[test]
    fn test_compare_field_values_wrap() {
        let modulus = BigUint::from(1000003u64);
        let a = vec![BigUint::zero()];
        let b = vec![&modulus - &BigUint::one()];
        assert!(compare_field_values(&a, &b, &modulus, 1));
    }
}
