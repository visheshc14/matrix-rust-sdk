// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This crate implements methods to parse and generate QR codes that are used
//! for interactive verification in [Matrix](https://matrix.org/).
//!
//! It implements the QR format defined in the Matrix [spec].
//!
//! [spec]: https://spec.matrix.org/unstable/client-server-api/#qr-code-format
//!
//! ```no_run
//! # use matrix_qrcode::{QrVerification, DecodingError};
//! # fn main() -> Result<(), DecodingError> {
//! use image;
//!
//! let image = image::open("/path/to/my/image.png").unwrap();
//! let result = QrVerification::from_image(image)?;
//! # Ok(())
//! # }
//! ```

#![cfg_attr(feature = "docs", feature(doc_cfg))]
#![deny(
    missing_debug_implementations,
    dead_code,
    trivial_casts,
    missing_docs,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]

mod error;
mod types;
mod utils;

pub use error::{DecodingError, EncodingError};
#[cfg(feature = "decode_image")]
#[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
pub use image;
pub use qrcode;
#[cfg(feature = "decode_image")]
#[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
pub use rqrr;
pub use types::{
    QrVerification, SelfVerificationData, SelfVerificationNoMasterKey, VerificationData,
};

#[cfg(test)]
mod test {
    #[cfg(feature = "decode_image")]
    use std::{convert::TryFrom, io::Cursor};

    #[cfg(feature = "decode_image")]
    use image::{ImageFormat, Luma};
    #[cfg(feature = "decode_image")]
    use qrcode::QrCode;

    #[cfg(feature = "decode_image")]
    use crate::utils::decode_qr;
    use crate::{DecodingError, QrVerification};

    #[cfg(feature = "decode_image")]
    static VERIFICATION: &[u8; 4277] = include_bytes!("../data/verification.png");
    #[cfg(feature = "decode_image")]
    static SELF_VERIFICATION: &[u8; 1467] = include_bytes!("../data/self-verification.png");
    #[cfg(feature = "decode_image")]
    static SELF_NO_MASTER: &[u8; 1775] = include_bytes!("../data/self-no-master.png");

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_qr_test() {
        let image = Cursor::new(VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap().to_luma8();
        decode_qr(image).expect("Couldn't decode the QR code");
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_test() {
        let image = Cursor::new(VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap().to_luma8();
        let result = QrVerification::try_from(image).unwrap();

        assert!(matches!(result, QrVerification::Verification(_)));
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_encode_cycle() {
        let image = Cursor::new(VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap();
        let result = QrVerification::from_image(image).unwrap();

        assert!(matches!(result, QrVerification::Verification(_)));

        let encoded = result.to_qr_code().unwrap();
        let image = encoded.render::<Luma<u8>>().build();
        let second_result = QrVerification::try_from(image).unwrap();

        assert_eq!(result, second_result);

        let bytes = result.to_bytes().unwrap();
        let third_result = QrVerification::from_bytes(bytes).unwrap();

        assert_eq!(result, third_result);
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_encode_cycle_self() {
        let image = Cursor::new(SELF_VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap();
        let result = QrVerification::try_from(image).unwrap();

        assert!(matches!(result, QrVerification::SelfVerification(_)));

        let encoded = result.to_qr_code().unwrap();
        let image = encoded.render::<Luma<u8>>().build();
        let second_result = QrVerification::from_luma(image).unwrap();

        assert_eq!(result, second_result);

        let bytes = result.to_bytes().unwrap();
        let third_result = QrVerification::from_bytes(bytes).unwrap();

        assert_eq!(result, third_result);
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_encode_cycle_self_no_master() {
        let image = Cursor::new(SELF_NO_MASTER);
        let image = image::load(image, ImageFormat::Png).unwrap();
        let result = QrVerification::from_image(image).unwrap();

        assert!(matches!(result, QrVerification::SelfVerificationNoMasterKey(_)));

        let encoded = result.to_qr_code().unwrap();
        let image = encoded.render::<Luma<u8>>().build();
        let second_result = QrVerification::try_from(image).unwrap();

        assert_eq!(result, second_result);

        let bytes = result.to_bytes().unwrap();
        let third_result = QrVerification::try_from(bytes).unwrap();

        assert_eq!(result, third_result);
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_invalid_qr() {
        let qr = QrCode::new(b"NonMatrixCode").expect("Can't build a simple QR code");
        let image = qr.render::<Luma<u8>>().build();
        let result = QrVerification::try_from(image);
        assert!(matches!(result, Err(DecodingError::Header)))
    }

    #[test]
    fn decode_invalid_header() {
        let data = b"NonMatrixCode";
        let result = QrVerification::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Header)))
    }

    #[test]
    fn decode_invalid_mode() {
        let data = b"MATRIX\x02\x03";
        let result = QrVerification::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Mode(3))))
    }

    #[test]
    fn decode_invalid_version() {
        let data = b"MATRIX\x01\x03";
        let result = QrVerification::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Version(1))))
    }

    #[test]
    fn decode_missing_data() {
        let data = b"MATRIX\x02\x02";
        let result = QrVerification::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Read(_))))
    }

    #[test]
    fn decode_short_secret() {
        let data = b"MATRIX\
                   \x02\x02\x00\x07\
                   FLOW_ID\
                   AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                   BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
                   SECRET";

        let result = QrVerification::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::SharedSecret(_))))
    }

    #[test]
    fn decode_invalid_room_id() {
        let data = b"MATRIX\
                   \x02\x00\x00\x0f\
                   test:localhost\
                   AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                   BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
                   SECRETISLONGENOUGH";

        let result = QrVerification::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Identifier(_))))
    }
}
