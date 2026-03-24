//! 轻量级 percent-encode/decode 工具

/// Percent-decode 一个字符串（RFC 3986）。
///
/// 与 std::ffi::OsStr / percent-encoding crate 的区别：
/// - 专为 HTTP query-string 设计（+ 解码为空格）
/// - 把完整的 %XX 序列解码为原始字节，再统一 UTF-8 解码
/// - 如果结果不是有效 UTF-8（边界处截断的多字节序列），回退到原始字符串
pub(crate) fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        let b = bytes[pos];
        if b == b'%' && pos + 2 < bytes.len() {
            let hex = &s[pos + 1..pos + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                result.push(byte);
                pos += 3;
                continue;
            }
        } else if b == b'+' {
            result.push(b' ');
            pos += 1;
            continue;
        }
        result.push(b);
        pos += 1;
    }

    // 完整 UTF-8 解码成功才返回，否则保留原始字符串
    String::from_utf8(result).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_decode_ascii() {
        assert_eq!(percent_decode("hello"), "hello");
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("hello%20world"), "hello world");
    }

    #[test]
    fn test_percent_decode_chinese() {
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("query=%E4%B8%AD%E6%96%87"), "query=中文");
    }

    #[test]
    fn test_percent_decode_mixed() {
        assert_eq!(
            percent_decode("key=%E4%B8%AD%E6%96%87&lang=zh"),
            "key=中文&lang=zh"
        );
    }

    #[test]
    fn test_percent_decode_incomplete() {
        // 不完整的 %XX（如 %E4 是中文"中"的首字节，但没有后续字节）
        // → 不是有效 UTF-8，保留原始字符串
        assert_eq!(percent_decode("%E4"), "%E4");
        assert_eq!(percent_decode("%E4%B8"), "%E4%B8");
        assert_eq!(percent_decode("%E4%B8%AD%E6"), "%E4%B8%AD%E6");
    }

    #[test]
    fn test_percent_decode_invalid_hex() {
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
        assert_eq!(percent_decode("%GG"), "%GG");
    }

    #[test]
    fn test_percent_decode_empty() {
        assert_eq!(percent_decode(""), "");
        assert_eq!(percent_decode("+++"), "   ");
    }

    #[test]
    fn test_percent_decode_emoji() {
        assert_eq!(percent_decode("%F0%9F%98%84"), "😄");
    }

    #[test]
    fn test_percent_decode_query_string() {
        assert_eq!(
            percent_decode("name=%E5%BC%A0%E4%B8%89&desc=hello+world"),
            "name=张三&desc=hello world"
        );
    }

    #[test]
    fn test_percent_decode_e4_in_middle() {
        // %E4 在中间，后面紧跟有效 continuation bytes → 完整 UTF-8
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
    }
}
