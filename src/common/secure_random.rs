//! 密码学安全随机数（API Key、PKCE verifier 等秘密值）
//!
//! `fastrand` 是 wyrand，适合抖动、打散之类的非安全用途；它的状态可以从少量输出推回，
//! 不能用来生成别人拿到就能调用接口的秘密。这里直接从操作系统熵源取随机字节。

/// 从操作系统熵源填满 `buf`
///
/// 取不到系统熵（极罕见，通常是沙箱禁用了 getrandom 系统调用）时 panic：
/// 生成秘密时悄悄退回弱随机数比直接失败更糟。
pub fn fill_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("操作系统随机数源不可用，拒绝用弱随机数生成密钥");
}

/// 长度为 `len` 的随机字母数字串（62 个字符）
///
/// 用拒绝采样消除取模偏差：随机字节只接受 0..248（62 × 4），再对 62 取余。
pub fn alphanumeric(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const ACCEPT_BELOW: u8 = (256 / CHARSET.len() * CHARSET.len()) as u8;

    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        fill_bytes(&mut buf);
        for &b in &buf {
            if b < ACCEPT_BELOW {
                out.push(CHARSET[usize::from(b) % CHARSET.len()] as char);
                if out.len() == len {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphanumeric_has_requested_length_and_charset() {
        for len in [0, 1, 24, 32, 100] {
            let s = alphanumeric(len);
            assert_eq!(s.len(), len);
            assert!(s.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn alphanumeric_does_not_repeat() {
        assert_ne!(alphanumeric(32), alphanumeric(32));
    }

    #[test]
    fn fill_bytes_is_not_all_zero() {
        let mut buf = [0u8; 32];
        fill_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }
}
