// Copyright 2020 Ledger SAS
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

#![no_std]
#![no_main]
#![feature(const_fn)]
#![feature(min_const_generics)]

use nanos_sdk::buttons::ButtonEvent;
use nanos_sdk::ecc;
use nanos_sdk::io;
use nanos_sdk::io::StatusWords;
use nanos_sdk::nvm;
use nanos_sdk::random;
use nanos_sdk::PIC;
use nanos_ui::ui;
mod password;
use heapless::{consts::U64, Vec};
use password::{ArrayString, PasswordItem};
mod tinyaes;
use core::mem::MaybeUninit;

nanos_sdk::set_panic!(nanos_sdk::exiting_panic);

#[no_mangle]
#[link_section = ".nvm_data"]
/// Stores all passwords in Non-Volatile Memory
static mut PASSWORDS: PIC<nvm::Collection<PasswordItem, 128>> =
    PIC::new(nvm::Collection::new(PasswordItem::new()));

/// Possible characters for the randomly generated passwords
static PASS_CHARS: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// SLIP16 path for password encryption (used during export/import)
static BIP32_PATH: [u32; 2] = ecc::make_bip32_path(b"m/10016'/0");

enum Error {
    NoConsent,
    StorageFull,
}

#[no_mangle]
extern "C" fn sample_main() {
    let mut comm = io::Comm::new();

    // Don't use PASSWORDS directly in the program. It is static and using
    // it requires using unsafe everytime. Instead, take a reference here, so
    // in the rest of the program the borrow checker will be able to detect
    // missuses correctly.
    let mut passwords = unsafe { PASSWORDS.get_mut() };

    // Encryption/decryption key for import and export.
    let mut enc_key = [0u8; 32];
    ecc::bip32_derive(ecc::CurvesId::Secp256k1, &BIP32_PATH, &mut enc_key);

    loop {
        ui::SingleMessage::new("NanoPass").show();

        match comm.next_event() {
            io::Event::Button(ButtonEvent::BothButtonsRelease) => {
                nanos_sdk::exit_app(0)
            }
            io::Event::Button(_) => {}
            // Get version string
            io::Event::Command(0x01) => {
                const VERSION: &'static str = env!("CARGO_PKG_VERSION");
                comm.append(VERSION.as_bytes());
                comm.reply_ok();
            }
            // Get number of stored passwords
            io::Event::Command(0x02) => {
                let len: [u8; 4] = passwords.len().to_be_bytes();
                comm.append(&len);
                comm.reply_ok();
            }
            // Add a password
            // If P1 == 0, password is in the data
            // If P1 == 1, password must be generated by the device
            io::Event::Command(0x03) => {
                let name = ArrayString::<32>::from_bytes(comm.get(5, 5 + 32));
                let pass = match comm.get(2, 3)[0] {
                    0 => Some(ArrayString::<32>::from_bytes(
                        comm.get(5 + 32, 5 + 32 + 32),
                    )),
                    _ => None,
                };
                comm.reply(match set_password(passwords, &name, &pass) {
                    Ok(()) => StatusWords::OK,
                    Err(_) => StatusWords::Unknown,
                });
            }
            // Get password name
            // This is used by the client to list the names of stored password
            io::Event::Command(0x04) => {
                let mut index_bytes = [0; 4];
                index_bytes.copy_from_slice(comm.get(5, 5 + 4));
                let index = u32::from_be_bytes(index_bytes);
                match passwords.get(index as usize) {
                    Some(password) => {
                        comm.append(password.name.bytes());
                        comm.reply_ok()
                    }
                    None => comm.reply(StatusWords::Unknown),
                }
            }
            // Get password by name
            io::Event::Command(0x05) => {
                let name = ArrayString::<32>::from_bytes(comm.get(5, 5 + 32));

                match passwords.into_iter().find(|&&x| x.name == name) {
                    Some(&p) => {
                        if ui::MessageValidator::new(
                            &[name.as_str()],
                            &[&"Read", &"password"],
                            &[&"Cancel"],
                        )
                        .ask()
                        {
                            comm.append(p.pass.bytes());
                            comm.reply_ok();
                        } else {
                            comm.reply(StatusWords::Unknown);
                        }
                    }
                    None => {
                        // Password not found
                        comm.reply(StatusWords::Unknown);
                    }
                }
            }
            // Delete password by name
            io::Event::Command(0x06) => {
                let name = ArrayString::<32>::from_bytes(comm.get(5, 5 + 32));
                match passwords.into_iter().position(|x| x.name == name) {
                    Some(p) => {
                        if ui::MessageValidator::new(
                            &[name.as_str()],
                            &[&"Remove", &"password"],
                            &[&"Cancel"],
                        )
                        .ask()
                        {
                            passwords.remove(p);
                            comm.reply_ok();
                        } else {
                            comm.reply(StatusWords::Unknown);
                        }
                    }
                    None => {
                        // Password not found
                        comm.reply(StatusWords::Unknown);
                    }
                }
            }
            // Export
            // P1 can be 0 for plaintext, 1 for encrypted export.
            io::Event::Command(0x07) => match comm.get_p1() {
                0 => export(&mut comm, &passwords, None),
                1 => export(&mut comm, &passwords, Some(&enc_key)),
                _ => comm.reply(StatusWords::Unknown),
            },
            // Reserved for export
            io::Event::Command(0x08) => {
                comm.reply(StatusWords::Unknown);
            }
            // Import
            // P1 can be 0 for plaintext, 1 for encrypted import.
            io::Event::Command(0x09) => match comm.get_p1() {
                0 => import(&mut comm, &mut passwords, None),
                1 => import(&mut comm, &mut passwords, Some(&enc_key)),
                _ => comm.reply(StatusWords::Unknown),
            },
            // Reserved for import
            io::Event::Command(0x0a) => {
                comm.reply(StatusWords::Unknown);
            }
            io::Event::Command(0x0b) => {
                // Remove all passwords
                comm.reply(
                    if ui::MessageValidator::new(
                        &[],
                        &[&"Remove all", &"passwords"],
                        &[&"Cancel"],
                    )
                    .ask()
                    {
                        if ui::MessageValidator::new(
                            &[],
                            &[&"Are you", &"sure?"],
                            &[&"Cancel"],
                        )
                        .ask()
                        {
                            passwords.clear();
                            StatusWords::OK
                        } else {
                            StatusWords::Unknown
                        }
                    } else {
                        StatusWords::Unknown
                    },
                );
            }
            // Exit
            io::Event::Command(0x0c) => {
                comm.reply_ok();
                nanos_sdk::exit_app(0);
            }
            io::Event::Command(_) => comm.reply(StatusWords::BadCLA),
        }
    }
}

/// Generates a random password.
///
/// # Arguments
///
/// * `dest` - An array where the result is stored. Must be at least
///   `size` long. No terminal zero is written.
/// * `size` - The size of the password to be generated
fn generate_random_password(dest: &mut [u8], size: usize) {
    for item in dest.iter_mut().take(size) {
        let rand_index = random::rand_u32_range(0..PASS_CHARS.len() as u32);
        *item = PASS_CHARS.chars().nth(rand_index as usize).unwrap() as u8;
    }
}

/// Adds or update a password in the store.
/// Queries confirmation from the user in the UX.
///
/// # Arguments
///
/// * `name` - Slice to the new name of the password. Must be 32 bytes long.
///   Null terminated.
/// * `pass` - New password. If None, a password is generated automatically.
fn set_password(
    passwords: &mut nvm::Collection<PasswordItem, 128>,
    name: &ArrayString<32>,
    pass: &Option<ArrayString<32>>,
) -> Result<(), Error> {
    // Create the item to be added.
    let mut new_item = PasswordItem::new();
    new_item.name = *name;
    match pass {
        Some(a) => new_item.pass = *a,
        None => {
            let mut pass = [0u8; 16];
            let len = pass.len();
            generate_random_password(&mut pass, len);
            new_item.pass.set_from_bytes(&pass);
        }
    }

    return match passwords.into_iter().position(|x| x.name == *name) {
        Some(index) => {
            // A password with this name already exists.
            if !ui::MessageValidator::new(
                &[name.as_str()],
                &[&"Update", &"password"],
                &[&"Cancel"],
            )
            .ask()
            {
                return Err(Error::NoConsent);
            }
            passwords.remove(index);
            match passwords.add(&new_item) {
                Ok(()) => Ok(()),
                // We just removed a password, this should not happen
                Err(nvm::StorageFullError) => panic!(),
            }
        }
        None => {
            // Ask user confirmation
            if !ui::MessageValidator::new(
                &[name.as_str()],
                &[&"Create", &"password"],
                &[&"Cancel"],
            )
            .ask()
            {
                return Err(Error::NoConsent);
            }
            match passwords.add(&new_item) {
                Ok(()) => Ok(()),
                Err(nvm::StorageFullError) => Err(Error::StorageFull),
            }
        }
    };
}

/// Export procedure.
///
/// # Arguments
///
/// * `enc_key` - Encryption key. If None, passwords are exported in plaintext.
fn export(
    comm: &mut io::Comm,
    passwords: &nvm::Collection<PasswordItem, 128>,
    enc_key: Option<&[u8; 32]>,
) {
    // Ask user confirmation
    if !ui::MessageValidator::new(&[], &[&"Export", &"passwords"], &[&"Cancel"])
        .ask()
    {
        comm.reply(StatusWords::Unknown);
        return;
    }

    // If export is in plaintext, add a warning
    let encrypted = enc_key.is_some();
    if !encrypted
        && !ui::MessageValidator::new(
            &[&"Export is plaintext!"],
            &[&"Confirm"],
            &[&"Cancel"],
        )
        .ask()
    {
        comm.reply(StatusWords::Unknown);
        return;
    }

    // User accepted. Reply with the number of passwords
    let count = passwords.len();
    comm.append(&count.to_be_bytes());
    comm.reply_ok();

    // We are now waiting for N APDUs to retrieve all passwords.
    // If encryption is enabled, the IV is returned during the first iteration.
    ui::SingleMessage::new("Exporting...").show();

    let mut iter = passwords.into_iter();
    let mut next_item = iter.next();
    while next_item.is_some() {
        match comm.next_command() {
            // Fetch next password
            0x08 => {
                let password = next_item.unwrap();
                // If encryption is enabled, encrypt the buffer inplace.
                if encrypted {
                    let mut nonce = [0u8; 16];
                    random::rand_bytes(&mut nonce);
                    comm.append(&nonce);
                    let mut buffer: Vec<u8, U64> = Vec::new();
                    buffer.extend_from_slice(password.name.bytes()).unwrap();
                    buffer.extend_from_slice(password.pass.bytes()).unwrap();
                    // Encrypt buffer in AES-256-CBC with random IV
                    let mut aes_ctx = MaybeUninit::<tinyaes::AES_ctx>::uninit();
                    unsafe {
                        tinyaes::AES_init_ctx_iv(
                            aes_ctx.as_mut_ptr(),
                            enc_key.unwrap().as_ptr(),
                            nonce.as_ptr(),
                        );
                        tinyaes::AES_CBC_encrypt_buffer(
                            aes_ctx.as_mut_ptr(),
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                        );
                    }
                    comm.append(&buffer as &[u8]);
                    // Now calculate AES-256-CBC-MAC
                    unsafe {
                        tinyaes::AES_init_ctx_iv(
                            aes_ctx.as_mut_ptr(),
                            enc_key.unwrap().as_ptr(),
                            nonce.as_ptr(),
                        );
                        tinyaes::AES_CBC_encrypt_buffer(
                            aes_ctx.as_mut_ptr(),
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                        );
                    }
                    let mac = &buffer[buffer.len() - 16..];
                    comm.append(mac);
                } else {
                    comm.append(password.name.bytes());
                    comm.append(password.pass.bytes());
                }
                comm.reply_ok();
                // Advance iterator.
                next_item = iter.next();
            }
            _ => {}
        }
    }
}

/// Import procedure.
///
/// # Arguments
///
/// * `enc_key` - Encryption key. If None, passwords are imported as plaintext.
fn import(
    comm: &mut io::Comm,
    passwords: &mut nvm::Collection<PasswordItem, 128>,
    enc_key: Option<&[u8; 32]>,
) {
    let encrypted = enc_key.is_some();

    // Retrieve the number of passwords to be imported
    let mut count_bytes = [0u8; 4];
    count_bytes.copy_from_slice(comm.get(5, 5 + 4));
    let mut count = u32::from_be_bytes(count_bytes);
    // Ask user confirmation
    if !ui::MessageValidator::new(&[], &[&"Import", &"passwords"], &[&"Cancel"])
        .ask()
    {
        comm.reply(StatusWords::Unknown);
        return;
    } else {
        comm.reply_ok();
    }
    // Wait for all items
    ui::SingleMessage::new("Importing...").show();
    while count > 0 {
        match comm.next_command() {
            // Fetch next password
            0x0a => {
                count -= 1;
                let mut new_item = PasswordItem::new();
                let mut decrypt_failed = false;
                if encrypted {
                    let nonce = comm.get(5, 5 + 16);
                    let mut buffer: Vec<u8, U64> = Vec::new();
                    buffer
                        .extend_from_slice(comm.get(5 + 16, 5 + 16 + 64))
                        .unwrap();
                    // Decrypt with AES-256-CBC
                    let mut aes_ctx = MaybeUninit::<tinyaes::AES_ctx>::uninit();
                    unsafe {
                        tinyaes::AES_init_ctx_iv(
                            aes_ctx.as_mut_ptr(),
                            enc_key.unwrap().as_ptr(),
                            nonce.as_ptr(),
                        );
                        tinyaes::AES_CBC_decrypt_buffer(
                            aes_ctx.as_mut_ptr(),
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                        );
                    }
                    new_item.name =
                        ArrayString::<32>::from_bytes(&buffer[..32]);
                    new_item.pass =
                        ArrayString::<32>::from_bytes(&buffer[32..64]);
                    // Verify the MAC
                    buffer.clear();
                    buffer
                        .extend_from_slice(comm.get(5 + 16, 5 + 16 + 64))
                        .unwrap();
                    unsafe {
                        tinyaes::AES_init_ctx_iv(
                            aes_ctx.as_mut_ptr(),
                            enc_key.unwrap().as_ptr(),
                            nonce.as_ptr(),
                        );
                        tinyaes::AES_CBC_encrypt_buffer(
                            aes_ctx.as_mut_ptr(),
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                        );
                    }
                    let received_mac = comm.get(5 + 16 + 64, 5 + 16 + 64 + 16);
                    let expected_mac = &buffer[buffer.len() - 16..];
                    decrypt_failed = received_mac != expected_mac;
                } else {
                    new_item.name =
                        ArrayString::<32>::from_bytes(comm.get(5, 5 + 32));
                    new_item.pass =
                        ArrayString::<32>::from_bytes(comm.get(5 + 32, 5 + 64));
                }
                if !decrypt_failed {
                    if let Some(index) = passwords
                        .into_iter()
                        .position(|x| x.name == new_item.name)
                    {
                        passwords.remove(index);
                    }
                    comm.reply(match passwords.add(&new_item) {
                        Ok(()) => StatusWords::OK,
                        Err(nvm::StorageFullError) => StatusWords::Unknown,
                    });
                } else {
                    comm.reply(StatusWords::Unknown);
                    break;
                }
            }
            _ => {
                comm.reply(StatusWords::BadCLA);
                break;
            }
        }
    }
}
