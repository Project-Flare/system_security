// Copyright 2020, The Android Open Source Project
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

//! Implements TempDir which aids in creating an cleaning up temporary directories for testing.

use std::fs::{create_dir, remove_dir_all};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::{env::temp_dir, ops::Deref};

use android_system_keystore2::aidl::android::system::keystore2::{
    IKeystoreService::IKeystoreService,
    IKeystoreSecurityLevel::IKeystoreSecurityLevel,
};
use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
    ErrorCode::ErrorCode, IKeyMintDevice::IKeyMintDevice, SecurityLevel::SecurityLevel,
};
use android_security_authorization::aidl::android::security::authorization::IKeystoreAuthorization::IKeystoreAuthorization;

pub mod authorizations;
pub mod ffi_test_utils;
pub mod key_generations;
pub mod run_as;

static KS2_SERVICE_NAME: &str = "android.system.keystore2.IKeystoreService/default";
static AUTH_SERVICE_NAME: &str = "android.security.authorization";

/// Represents the lifecycle of a temporary directory for testing.
#[derive(Debug)]
pub struct TempDir {
    path: std::path::PathBuf,
    do_drop: bool,
}

impl TempDir {
    /// Creates a temporary directory with a name of the form <prefix>_NNNNN where NNNNN is a zero
    /// padded random number with 5 figures. The prefix must not contain file system separators.
    /// The location of the directory cannot be chosen.
    /// The directory with all of its content is removed from the file system when the resulting
    /// object gets dropped.
    pub fn new(prefix: &str) -> std::io::Result<Self> {
        let tmp = loop {
            let mut tmp = temp_dir();
            let number: u16 = rand::random();
            tmp.push(format!("{}_{:05}", prefix, number));
            match create_dir(&tmp) {
                Err(e) => match e.kind() {
                    ErrorKind::AlreadyExists => continue,
                    _ => return Err(e),
                },
                Ok(()) => break tmp,
            }
        };
        Ok(Self { path: tmp, do_drop: true })
    }

    /// Returns the absolute path of the temporary directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a path builder for convenient extension of the path.
    ///
    /// ## Example:
    ///
    /// ```
    /// let tdir = TempDir::new("my_test")?;
    /// let temp_foo_bar = tdir.build().push("foo").push("bar");
    /// ```
    /// `temp_foo_bar` derefs to a Path that represents "<tdir.path()>/foo/bar"
    pub fn build(&self) -> PathBuilder {
        PathBuilder(self.path.clone())
    }

    /// When a test is failing you can set this to false in order to inspect
    /// the directory structure after the test failed.
    #[allow(dead_code)]
    pub fn do_not_drop(&mut self) {
        println!("Disabled automatic cleanup for: {:?}", self.path);
        log::info!("Disabled automatic cleanup for: {:?}", self.path);
        self.do_drop = false;
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.do_drop {
            remove_dir_all(&self.path).expect("Cannot delete temporary dir.");
        }
    }
}

/// Allows for convenient building of paths from a TempDir. See TempDir.build() for more details.
pub struct PathBuilder(PathBuf);

impl PathBuilder {
    /// Adds another segment to the end of the path. Consumes, modifies and returns self.
    pub fn push(mut self, segment: &str) -> Self {
        self.0.push(segment);
        self
    }
}

impl Deref for PathBuilder {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Get Keystore2 service.
pub fn get_keystore_service() -> binder::Strong<dyn IKeystoreService> {
    binder::get_interface(KS2_SERVICE_NAME).unwrap()
}

/// Get Keystore auth service.
pub fn get_keystore_auth_service() -> binder::Strong<dyn IKeystoreAuthorization> {
    binder::get_interface(AUTH_SERVICE_NAME).unwrap()
}

/// Security level-specific data.
pub struct SecLevel {
    /// Binder connection for the top-level service.
    pub keystore2: binder::Strong<dyn IKeystoreService>,
    /// Binder connection for the security level.
    pub binder: binder::Strong<dyn IKeystoreSecurityLevel>,
    /// Security level.
    pub level: SecurityLevel,
}

impl SecLevel {
    /// Return security level data for TEE.
    pub fn tee() -> Self {
        let level = SecurityLevel::TRUSTED_ENVIRONMENT;
        let keystore2 = get_keystore_service();
        let binder =
            keystore2.getSecurityLevel(level).expect("TEE security level should always be present");
        Self { keystore2, binder, level }
    }
    /// Return security level data for StrongBox, if present.
    pub fn strongbox() -> Option<Self> {
        let level = SecurityLevel::STRONGBOX;
        let keystore2 = get_keystore_service();
        match key_generations::map_ks_error(keystore2.getSecurityLevel(level)) {
            Ok(binder) => Some(Self { keystore2, binder, level }),
            Err(e) => {
                assert_eq!(e, key_generations::Error::Km(ErrorCode::HARDWARE_TYPE_UNAVAILABLE));
                None
            }
        }
    }
    /// Indicate whether this security level is a KeyMint implementation (not Keymaster).
    pub fn is_keymint(&self) -> bool {
        let instance = match self.level {
            SecurityLevel::TRUSTED_ENVIRONMENT => "default",
            SecurityLevel::STRONGBOX => "strongbox",
            l => panic!("unexpected level {l:?}"),
        };
        let name = format!("android.hardware.security.keymint.IKeyMintDevice/{instance}");
        binder::is_declared(&name).expect("Could not check for declared keymint interface")
    }

    /// Indicate whether this security level is a Keymaster implementation (not KeyMint).
    pub fn is_keymaster(&self) -> bool {
        !self.is_keymint()
    }

    /// Get KeyMint version.
    /// Returns 0 if the underlying device is Keymaster not KeyMint.
    pub fn get_keymint_version(&self) -> i32 {
        let instance = match self.level {
            SecurityLevel::TRUSTED_ENVIRONMENT => "default",
            SecurityLevel::STRONGBOX => "strongbox",
            l => panic!("unexpected level {l:?}"),
        };
        let name = format!("android.hardware.security.keymint.IKeyMintDevice/{instance}");
        if binder::is_declared(&name).expect("Could not check for declared keymint interface") {
            let km: binder::Strong<dyn IKeyMintDevice> = binder::get_interface(&name).unwrap();
            km.getInterfaceVersion().unwrap()
        } else {
            0
        }
    }
}
