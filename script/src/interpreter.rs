use std::{cmp, mem};
use bytes::Bytes;
use keys::{Signature, Public};
use chain::SEQUENCE_LOCKTIME_DISABLE_FLAG;
use crypto::{sha1, sha256, dhash160, dhash256, ripemd160};
use {
	script, Script, Num, VerificationFlags, Opcode, Error,
	Sighash, SignatureChecker, SignatureVersion, Stack
};

/// Helper function.
fn check_signature(
	checker: &SignatureChecker,
	mut script_sig: Vec<u8>,
	public: Vec<u8>,
	script_code: &Script,
	version: SignatureVersion
) -> bool {
	let public = match Public::from_slice(&public) {
		Ok(public) => public,
		_ => return false,
	};

	if script_sig.is_empty() {
		return false;
	}

	let hash_type = script_sig.pop().unwrap() as u32;
	let signature = script_sig.into();

	checker.check_signature(&signature, &public, script_code, hash_type, version)
}

fn is_public_key(v: &[u8]) -> bool {
	match v.len() {
		33 if v[0] == 2 || v[0] == 3 => true,
		65 if v[0] == 4 => true,
		_ => false,
	}
}

/// A canonical signature exists of: <30> <total len> <02> <len R> <R> <02> <len S> <S> <hashtype>
/// Where R and S are not negative (their first byte has its highest bit not set), and not
/// excessively padded (do not start with a 0 byte, unless an otherwise negative number follows,
/// in which case a single 0 byte is necessary and even required).
///
/// See https://bitcointalk.org/index.php?topic=8392.msg127623#msg127623
///
/// This function is consensus-critical since BIP66.
fn is_valid_signature_encoding(sig: &[u8]) -> bool {
	// Format: 0x30 [total-length] 0x02 [R-length] [R] 0x02 [S-length] [S] [sighash]
	// * total-length: 1-byte length descriptor of everything that follows,
	//   excluding the sighash byte.
	// * R-length: 1-byte length descriptor of the R value that follows.
	// * R: arbitrary-length big-endian encoded R value. It must use the shortest
	//   possible encoding for a positive integers (which means no null bytes at
	//   the start, except a single one when the next byte has its highest bit set).
	// * S-length: 1-byte length descriptor of the S value that follows.
	// * S: arbitrary-length big-endian encoded S value. The same rules apply.
	// * sighash: 1-byte value indicating what data is hashed (not part of the DER
	//   signature)

	// Minimum and maximum size constraints
	if sig.len() < 9 || sig.len() > 73 {
		return false;
	}

	// A signature is of type 0x30 (compound)
	if sig[0] != 0x30 {
		return false;
	}

	// Make sure the length covers the entire signature.
	if sig[1] as usize != sig.len() - 3 {
		return false;
	}

	// Extract the length of the R element.
	let len_r = sig[3] as usize;

	// Make sure the length of the S element is still inside the signature.
	if len_r + 5 >= sig.len() {
		return false;
	}

	// Extract the length of the S element.
	let len_s = sig[len_r + 5] as usize;

	// Verify that the length of the signature matches the sum of the length
	if len_r + len_s + 7 != sig.len() {
		return false;
	}

	// Check whether the R element is an integer.
	if sig[2] != 2 {
		return false;
	}

	// Zero-length integers are not allowed for R.
	if len_r == 0 {
		return false;
	}

	// Negative numbers are not allowed for R.
	if (sig[4] & 0x80) != 0 {
		return false;
	}

	// Null bytes at the start of R are not allowed, unless R would
	// otherwise be interpreted as a negative number.
	if len_r > 1 && sig[4] == 0 && (!(sig[5] & 0x80)) != 0 {
		return false;
	}

	// Check whether the S element is an integer.
	if sig[len_r + 4] != 2 {
		return false;
	}

	// Zero-length integers are not allowed for S.
	if len_s == 0 {
		return false;
	}

	// Negative numbers are not allowed for S.
	if (sig[len_r + 6] & 0x80) != 0 {
		return false;
	}

	// Null bytes at the start of S are not allowed, unless S would otherwise be
	// interpreted as a negative number.
	if len_s > 1 && (sig[len_r + 6] == 0) && (!(sig[len_r + 7] & 0x80)) != 0 {
		return false;
	}

	true
}

fn is_low_der_signature(sig: &[u8]) -> Result<(), Error> {
	if !is_valid_signature_encoding(sig) {
		return Err(Error::SignatureDer);
	}

	let signature: Signature = sig.into();
	if !signature.check_low_s() {
		return Err(Error::SignatureHighS);
	}

	Ok(())
}

fn is_defined_hashtype_signature(sig: &[u8]) -> bool {
	if sig.is_empty() {
		return false;
	}

	Sighash::is_defined(sig[sig.len() -1] as u32)
}

fn check_signature_encoding(sig: &[u8], flags: &VerificationFlags) -> Result<(), Error> {
	// Empty signature. Not strictly DER encoded, but allowed to provide a
	// compact way to provide an invalid signature for use with CHECK(MULTI)SIG

	if sig.is_empty() {
		return Ok(());
	}

	if (flags.verify_dersig || flags.verify_low_s || flags.verify_strictenc) && !is_valid_signature_encoding(sig) {
		return Err(Error::SignatureDer);
	}

	if flags.verify_low_s {
		try!(is_low_der_signature(sig));
	}

	if flags.verify_strictenc && !is_defined_hashtype_signature(sig) {
		Err(Error::SignatureHashtype)
	} else {
		Ok(())
	}
}

fn check_pubkey_encoding(v: &[u8], flags: &VerificationFlags) -> Result<(), Error> {
	if flags.verify_strictenc && !is_public_key(v) {
		return Err(Error::PubkeyType);
	}

	Ok(())
}

fn check_minimal_push(data: &[u8], opcode: Opcode) -> bool {
	if data.is_empty() {
		// Could have used OP_0.
		opcode == Opcode::OP_0
	} else if data.len() == 1 && data[0] >= 1 && data[0] <= 16 {
		// Could have used OP_1 .. OP_16.
		opcode as u8 == Opcode::OP_1 as u8 + (data[0] - 1)
	} else if data.len() == 1 && data[0] == 0x81 {
		// Could have used OP_1NEGATE
		opcode == Opcode::OP_1NEGATE
	} else if data.len() <= 75 {
		// Could have used a direct push (opcode indicating number of bytes pushed + those bytes).
		opcode as usize == data.len()
	} else if data.len() <= 255 {
		// Could have used OP_PUSHDATA.
		opcode == Opcode::OP_PUSHDATA1
	} else if data.len() <= 65535 {
		// Could have used OP_PUSHDATA2.
		opcode == Opcode::OP_PUSHDATA2
	} else {
		true
	}
}

fn cast_to_bool(data: &[u8]) -> bool {
	if data.is_empty() {
		return false;
	}

	if data[..data.len() - 1].iter().any(|x| x != &0) {
		return true;
	}

	let last = data[data.len() - 1];
	if last == 0 || last == 0x80 {
		false
	} else {
		true
	}
}

pub fn verify_script(
	script_sig: &Script,
	script_pubkey: &Script,
	flags: &VerificationFlags,
	checker: &SignatureChecker
) -> Result<(), Error> {
	if flags.verify_sigpushonly && !script_sig.is_push_only() {
		return Err(Error::SignaturePushOnly);
	}

	let mut stack = Stack::new();
	let mut stack_copy = Stack::new();

	try!(eval_script(&mut stack, script_sig, flags, checker, SignatureVersion::Base));

	if flags.verify_p2sh {
		stack_copy = stack.clone();
	}

	let res = try!(eval_script(&mut stack, script_pubkey, flags, checker, SignatureVersion::Base));
	if !res {
		return Err(Error::EvalFalse);
	}

    // Additional validation for spend-to-script-hash transactions:
	if flags.verify_p2sh && script_pubkey.is_pay_to_script_hash() {
		if !script_sig.is_push_only() {
			return Err(Error::SignaturePushOnly);
		}

		mem::swap(&mut stack, &mut stack_copy);

        // stack cannot be empty here, because if it was the
        // P2SH  HASH <> EQUAL  scriptPubKey would be evaluated with
        // an empty stack and the EvalScript above would return false.
        assert!(!stack.is_empty());

		let pubkey2: Script = try!(stack.pop()).into();

		let res = try!(eval_script(&mut stack, &pubkey2, flags, checker, SignatureVersion::Base));
		if !res {
			return Err(Error::EvalFalse);
		}
	}

    // The CLEANSTACK check is only performed after potential P2SH evaluation,
    // as the non-P2SH evaluation of a P2SH script will obviously not result in
    // a clean stack (the P2SH inputs remain). The same holds for witness evaluation.
	if flags.verify_cleanstack {
        // Disallow CLEANSTACK without P2SH, as otherwise a switch CLEANSTACK->P2SH+CLEANSTACK
        // would be possible, which is not a softfork (and P2SH should be one).
		assert!(flags.verify_p2sh);
		assert!(flags.verify_witness);
		if stack.len() != 1 {
			return Err(Error::Cleanstack);
		}
	}

	Ok(())
}

pub fn eval_script(
	stack: &mut Stack<Bytes>,
	script: &Script,
	flags: &VerificationFlags,
	checker: &SignatureChecker,
	version: SignatureVersion
) -> Result<bool, Error> {
	if script.len() > script::MAX_SCRIPT_SIZE {
		return Err(Error::ScriptSize);
	}

	let mut pc = 0;
	let mut op_count = 0;
	let mut begincode = 0;
	let mut exec_stack = Vec::<bool>::new();
	let mut altstack = Stack::<Bytes>::new();

	while pc < script.len() {
		let executing = exec_stack.iter().all(|x| *x);
		let instruction = try!(script.get_instruction(pc));
		let opcode = instruction.opcode;

		if let Some(data) = instruction.data {
			if data.len() > script::MAX_SCRIPT_ELEMENT_SIZE {
				return Err(Error::PushSize);
			}

			if executing && flags.verify_minimaldata && !check_minimal_push(data, opcode) {
				return Err(Error::Minimaldata);
			}
		}

		if opcode.is_countable() {
			op_count += 1;
			if op_count > script::MAX_OPS_PER_SCRIPT {
				return Err(Error::OpCount);
			}
		}

		if opcode.is_disabled() {
			return Err(Error::DisabledOpcode(opcode));
		}

		if !(executing || (Opcode::OP_IF <= opcode && opcode <= Opcode::OP_ENDIF)) {
			pc += instruction.step;
			continue;
		}

		match opcode {
			Opcode::OP_PUSHDATA1 |
			Opcode::OP_PUSHDATA2 |
			Opcode::OP_PUSHDATA4 |
			Opcode::OP_0 |
			Opcode::OP_PUSHBYTES_1 |
			Opcode::OP_PUSHBYTES_2 |
			Opcode::OP_PUSHBYTES_3 |
			Opcode::OP_PUSHBYTES_4 |
			Opcode::OP_PUSHBYTES_5 |
			Opcode::OP_PUSHBYTES_6 |
			Opcode::OP_PUSHBYTES_7 |
			Opcode::OP_PUSHBYTES_8 |
			Opcode::OP_PUSHBYTES_9 |
			Opcode::OP_PUSHBYTES_10 |
			Opcode::OP_PUSHBYTES_11 |
			Opcode::OP_PUSHBYTES_12 |
			Opcode::OP_PUSHBYTES_13 |
			Opcode::OP_PUSHBYTES_14 |
			Opcode::OP_PUSHBYTES_15 |
			Opcode::OP_PUSHBYTES_16 |
			Opcode::OP_PUSHBYTES_17 |
			Opcode::OP_PUSHBYTES_18 |
			Opcode::OP_PUSHBYTES_19 |
			Opcode::OP_PUSHBYTES_20 |
			Opcode::OP_PUSHBYTES_21 |
			Opcode::OP_PUSHBYTES_22 |
			Opcode::OP_PUSHBYTES_23 |
			Opcode::OP_PUSHBYTES_24 |
			Opcode::OP_PUSHBYTES_25 |
			Opcode::OP_PUSHBYTES_26 |
			Opcode::OP_PUSHBYTES_27 |
			Opcode::OP_PUSHBYTES_28 |
			Opcode::OP_PUSHBYTES_29 |
			Opcode::OP_PUSHBYTES_30 |
			Opcode::OP_PUSHBYTES_31 |
			Opcode::OP_PUSHBYTES_32 |
			Opcode::OP_PUSHBYTES_33 |
			Opcode::OP_PUSHBYTES_34 |
			Opcode::OP_PUSHBYTES_35 |
			Opcode::OP_PUSHBYTES_36 |
			Opcode::OP_PUSHBYTES_37 |
			Opcode::OP_PUSHBYTES_38 |
			Opcode::OP_PUSHBYTES_39 |
			Opcode::OP_PUSHBYTES_40 |
			Opcode::OP_PUSHBYTES_41 |
			Opcode::OP_PUSHBYTES_42 |
			Opcode::OP_PUSHBYTES_43 |
			Opcode::OP_PUSHBYTES_44 |
			Opcode::OP_PUSHBYTES_45 |
			Opcode::OP_PUSHBYTES_46 |
			Opcode::OP_PUSHBYTES_47 |
			Opcode::OP_PUSHBYTES_48 |
			Opcode::OP_PUSHBYTES_49 |
			Opcode::OP_PUSHBYTES_50 |
			Opcode::OP_PUSHBYTES_51 |
			Opcode::OP_PUSHBYTES_52 |
			Opcode::OP_PUSHBYTES_53 |
			Opcode::OP_PUSHBYTES_54 |
			Opcode::OP_PUSHBYTES_55 |
			Opcode::OP_PUSHBYTES_56 |
			Opcode::OP_PUSHBYTES_57 |
			Opcode::OP_PUSHBYTES_58 |
			Opcode::OP_PUSHBYTES_59 |
			Opcode::OP_PUSHBYTES_60 |
			Opcode::OP_PUSHBYTES_61 |
			Opcode::OP_PUSHBYTES_62 |
			Opcode::OP_PUSHBYTES_63 |
			Opcode::OP_PUSHBYTES_64 |
			Opcode::OP_PUSHBYTES_65 |
			Opcode::OP_PUSHBYTES_66 |
			Opcode::OP_PUSHBYTES_67 |
			Opcode::OP_PUSHBYTES_68 |
			Opcode::OP_PUSHBYTES_69 |
			Opcode::OP_PUSHBYTES_70 |
			Opcode::OP_PUSHBYTES_71 |
			Opcode::OP_PUSHBYTES_72 |
			Opcode::OP_PUSHBYTES_73 |
			Opcode::OP_PUSHBYTES_74 |
			Opcode::OP_PUSHBYTES_75 => {
				if let Some(data) = instruction.data {
					stack.push(data.to_vec().into());
				}
			},
			Opcode::OP_1NEGATE |
			Opcode::OP_1 |
			Opcode::OP_2 |
			Opcode::OP_3 |
			Opcode::OP_4 |
			Opcode::OP_5 |
			Opcode::OP_6 |
			Opcode::OP_7 |
			Opcode::OP_8 |
			Opcode::OP_9 |
			Opcode::OP_10 |
			Opcode::OP_11 |
			Opcode::OP_12 |
			Opcode::OP_13 |
			Opcode::OP_14 |
			Opcode::OP_15 |
			Opcode::OP_16 => {
				let value = opcode as u8 - (Opcode::OP_1 as u8 - 1);
				stack.push(Num::from(value).to_bytes());
			},
			Opcode::OP_CAT | Opcode::OP_SUBSTR | Opcode::OP_LEFT | Opcode::OP_RIGHT |
			Opcode::OP_INVERT | Opcode::OP_AND | Opcode::OP_OR | Opcode::OP_XOR |
			Opcode::OP_2MUL | Opcode::OP_2DIV | Opcode::OP_MUL | Opcode::OP_DIV |
			Opcode::OP_MOD | Opcode::OP_LSHIFT | Opcode::OP_RSHIFT => {
				return Err(Error::DisabledOpcode(opcode));
			},
			Opcode::OP_NOP => break,
			Opcode::OP_CHECKLOCKTIMEVERIFY => {
				if !flags.verify_clocktimeverify {
					if flags.verify_discourage_upgradable_nops {
						return Err(Error::DiscourageUpgradableNops);
					}
				}

				// Note that elsewhere numeric opcodes are limited to
				// operands in the range -2**31+1 to 2**31-1, however it is
				// legal for opcodes to produce results exceeding that
				// range. This limitation is implemented by CScriptNum's
				// default 4-byte limit.
				//
				// If we kept to that limit we'd have a year 2038 problem,
				// even though the nLockTime field in transactions
				// themselves is uint32 which only becomes meaningless
				// after the year 2106.
				//
				// Thus as a special case we tell CScriptNum to accept up
				// to 5-byte bignums, which are good until 2**39-1, well
				// beyond the 2**32-1 limit of the nLockTime field itself.
				let lock_time = try!(Num::from_slice(try!(stack.last()), flags.verify_minimaldata, 5));

				// In the rare event that the argument may be < 0 due to
				// some arithmetic being done first, you can always use
				// 0 MAX CHECKLOCKTIMEVERIFY.
				if lock_time.is_negative() {
					return Err(Error::NegativeLocktime);
				}

				if !checker.check_lock_time(lock_time) {
					return Err(Error::UnsatisfiedLocktime);
				}
			},
			Opcode::OP_CHECKSEQUENCEVERIFY => {
				if !flags.verify_chechsequenceverify {
					if flags.verify_discourage_upgradable_nops {
						return Err(Error::DiscourageUpgradableNops);
					}
				}

				let sequence = try!(Num::from_slice(try!(stack.last()), flags.verify_minimaldata, 5));

				if sequence.is_negative() {
					return Err(Error::NegativeLocktime);
				}

				if (sequence & (SEQUENCE_LOCKTIME_DISABLE_FLAG as i64).into()).is_zero() {
					if !checker.check_sequence(sequence) {
						return Err(Error::UnsatisfiedLocktime);
					}
				}
			},
			Opcode::OP_NOP1 |
			Opcode::OP_NOP4 |
			Opcode::OP_NOP5 |
			Opcode::OP_NOP6 |
			Opcode::OP_NOP7 |
			Opcode::OP_NOP8 |
			Opcode::OP_NOP9 |
			Opcode::OP_NOP10 => {
				if flags.verify_discourage_upgradable_nops {
					return Err(Error::DiscourageUpgradableNops);
				}
			},
			Opcode::OP_IF | Opcode::OP_NOTIF => {
				let mut exec_value = false;
				if executing {
					exec_value = cast_to_bool(&try!(stack.pop().map_err(|_| Error::UnbalancedConditional)));
					if opcode == Opcode::OP_NOTIF {
						exec_value = !exec_value;
					}
				}
				exec_stack.push(exec_value);
			},
			Opcode::OP_ELSE => {
				if exec_stack.is_empty() {
					return Err(Error::UnbalancedConditional);
				}
				let last = exec_stack[exec_stack.len() - 1];
				exec_stack[exec_stack.len() - 1] == !last;
			},
			Opcode::OP_ENDIF => {
				if exec_stack.is_empty() {
					return Err(Error::UnbalancedConditional);
				}
				exec_stack.pop();
			},
			Opcode::OP_VERIFY => {
				let exec_value = cast_to_bool(&try!(stack.pop()));
				if !exec_value {
					return Err(Error::Verify);
				}
			},
			Opcode::OP_RETURN => {
				return Err(Error::ReturnOpcode);
			},
			Opcode::OP_TOALTSTACK => {
				altstack.push(try!(stack.pop()));
			},
			Opcode::OP_FROMALTSTACK => {
				stack.push(try!(altstack.pop().map_err(|_| Error::InvalidAltstackOperation)));
			},
			Opcode::OP_2DROP => {
				try!(stack.drop(2));
			},
			Opcode::OP_2DUP => {
				try!(stack.dup(2));
			},
			Opcode::OP_3DUP => {
				try!(stack.dup(3));
			},
			Opcode::OP_2OVER => {
				try!(stack.over(2));
			},
			Opcode::OP_2ROT => {
				try!(stack.rot(2));
			},
			Opcode::OP_2SWAP => {
				try!(stack.swap(2));
			},
			Opcode::OP_IFDUP => {
				if cast_to_bool(try!(stack.last())) {
					try!(stack.dup(1));
				}
			},
			Opcode::OP_DEPTH => {
				let depth = Num::from(stack.len());
				stack.push(depth.to_bytes());
			},
			Opcode::OP_DROP => {
				try!(stack.pop());
			},
			Opcode::OP_DUP => {
				try!(stack.dup(1));
			},
			Opcode::OP_NIP => {
				try!(stack.nip());
			},
			Opcode::OP_OVER => {
				try!(stack.over(1));
			},
			Opcode::OP_PICK | Opcode::OP_ROLL => {
				let n: i64 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4)).into();
				if n < 0 || n >= stack.len() as i64 {
					return Err(Error::InvalidStackOperation);
				}

				let v = match opcode {
					Opcode::OP_PICK => try!(stack.top(n as usize)).clone(),
					_ => try!(stack.remove(n as usize)),
				};

				stack.push(v);
			},
			Opcode::OP_ROT => {
				try!(stack.rot(1));
			},
			Opcode::OP_SWAP => {
				try!(stack.swap(1));
			},
			Opcode::OP_TUCK => {
				try!(stack.tuck());
			},
			Opcode::OP_SIZE => {
				let n = Num::from(try!(stack.last()).len());
				stack.push(n.to_bytes());
			},
			Opcode::OP_EQUAL => {
				let v1 = try!(stack.pop());
				let v2 = try!(stack.pop());
				let to_push = match v1 == v2 {
					true => vec![1],
					false => vec![0],
				};
				stack.push(to_push.into());
			},
			Opcode::OP_EQUALVERIFY => {
				let equal = try!(stack.pop()) == try!(stack.pop());
				if !equal {
					return Err(Error::EqualVerify);
				}
			},
			Opcode::OP_1ADD => {
				let n = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4)) + 1.into();
				stack.push(n.to_bytes());
			},
			Opcode::OP_1SUB => {
				let n = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4)) - 1.into();
				stack.push(n.to_bytes());
			},
			Opcode::OP_NEGATE => {
				let n = -try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				stack.push(n.to_bytes());
			},
			Opcode::OP_ABS => {
				let n = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4)).abs();
				stack.push(n.to_bytes());
			},
			Opcode::OP_NOT => {
				let n = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4)).is_zero();
				let n = Num::from(n);
				stack.push(n.to_bytes());
			},
			Opcode::OP_0NOTEQUAL => {
				let n = !try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4)).is_zero();
				let n = Num::from(n);
				stack.push(n.to_bytes());
			},
			Opcode::OP_ADD => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				stack.push((v1 + v2).to_bytes());
			},
			Opcode::OP_SUB => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				stack.push((v2 - v1).to_bytes());
			},
			Opcode::OP_BOOLAND => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(!v1.is_zero() && !v2.is_zero());
				stack.push(v.to_bytes());
			},
			Opcode::OP_BOOLOR => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(!v1.is_zero() || !v2.is_zero());
				stack.push(v.to_bytes());
			},
			Opcode::OP_NUMEQUAL => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(v1 == v2);
				stack.push(v.to_bytes());
			},
			Opcode::OP_NUMEQUALVERIFY => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				if v1 != v2 {
					return Err(Error::NumEqualVerify);
				}
			},
			Opcode::OP_NUMNOTEQUAL => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(v1 != v2);
				stack.push(v.to_bytes());
			},
			Opcode::OP_LESSTHAN => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(v1 > v2);
				stack.push(v.to_bytes());
			},
			Opcode::OP_GREATERTHAN => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(v1 < v2);
				stack.push(v.to_bytes());
			},
			Opcode::OP_LESSTHANOREQUAL => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(v1 >= v2);
				stack.push(v.to_bytes());
			},
			Opcode::OP_GREATERTHANOREQUAL => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v = Num::from(v1 <= v2);
				stack.push(v.to_bytes());
			},
			Opcode::OP_MIN => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				stack.push(cmp::min(v1, v2).to_bytes());
			},
			Opcode::OP_MAX => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				stack.push(cmp::max(v1, v2).to_bytes());
			},
			Opcode::OP_WITHIN => {
				let v1 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v2 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let v3 = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				let to_push = match v2 <= v3 && v3 <= v1 {
					true => vec![1],
					false => vec![0],
				};
				stack.push(to_push.into());
			},
			Opcode::OP_RIPEMD160 => {
				let v = ripemd160(&try!(stack.pop()));
				stack.push(v.to_vec().into());
			},
			Opcode::OP_SHA1 => {
				let v = sha1(&try!(stack.pop()));
				stack.push(v.to_vec().into());
			},
			Opcode::OP_SHA256 => {
				let v = sha256(&try!(stack.pop()));
				stack.push(v.to_vec().into());
			},
			Opcode::OP_HASH160 => {
				let v = dhash160(&try!(stack.pop()));
				stack.push(v.to_vec().into());
			},
			Opcode::OP_HASH256 => {
				let v = dhash256(&try!(stack.pop()));
				stack.push(v.to_vec().into());
			},
			Opcode::OP_CODESEPARATOR => {
				begincode = pc;
			},
			Opcode::OP_CHECKSIG | Opcode::OP_CHECKSIGVERIFY => {
				let pubkey = try!(stack.pop());
				let signature = try!(stack.pop());
				let mut subscript = script.subscript(begincode);
				if version == SignatureVersion::Base {
					subscript = script.find_and_delete(&signature);
				}

				try!(check_signature_encoding(&signature, flags));
				try!(check_pubkey_encoding(&pubkey, flags));

				let success = check_signature(checker, signature.into(), pubkey.into(), &subscript, version);
				match opcode {
					Opcode::OP_CHECKSIG => {
						let to_push = match success {
							true => vec![1],
							false => vec![0],
						};
						stack.push(to_push.into());
					},
					Opcode::OP_CHECKSIGVERIFY if !success => {
						return Err(Error::CheckSigVerify);
					},
					_ => {},
				}
			},
			Opcode::OP_CHECKMULTISIG | Opcode::OP_CHECKMULTISIGVERIFY => {
				let keys_count = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				if keys_count < 0.into() || keys_count > script::MAX_PUBKEYS_PER_MULTISIG.into() {
					return Err(Error::PubkeyCount);
				}

				let keys_count: usize = keys_count.into();
				let keys: Vec<_> = try!((0..keys_count).into_iter().map(|_| stack.pop()).rev().collect());

				let sigs_count = try!(Num::from_slice(&try!(stack.pop()), flags.verify_minimaldata, 4));
				if sigs_count < 0.into() || sigs_count > keys_count.into() {
					return Err(Error::SigCount);
				}

				let sigs_count: usize = sigs_count.into();
				let sigs: Vec<_> = try!((0..sigs_count).into_iter().map(|_| stack.pop()).rev().collect());

				let mut subscript = script.subscript(begincode);

				if version == SignatureVersion::Base {
					for signature in &sigs {
						subscript = subscript.find_and_delete(signature);
					}
				}

				let mut success = true;
				let mut k = 0;
				let mut s = 0;
				while s < sigs.len() && success {
					// TODO: remove redundant copying
					let key = keys[k].clone();
					let sig = sigs[s].clone();

					try!(check_signature_encoding(&sig, flags));
					try!(check_pubkey_encoding(&key, flags));

					let ok = check_signature(checker, sig.into(), key.into(), &subscript, version);
					if ok {
						s += 1;
					}
					k += 1;

					success = sigs.len() - s <= keys.len() - k;
				}

				if !try!(stack.pop()).is_empty() && flags.verify_nulldummy {
					return Err(Error::SignatureNullDummy);
				}

				match opcode {
					Opcode::OP_CHECKMULTISIG => {
						let to_push = match success {
							true => vec![1],
							false => vec![0],
						};
						stack.push(to_push.into());
					},
					Opcode::OP_CHECKMULTISIGVERIFY if !success => {
						return Err(Error::CheckSigVerify);
					},
					_ => {},
				}
			},
			Opcode::OP_RESERVED |
			Opcode::OP_VER |
			Opcode::OP_RESERVED1 |
			Opcode::OP_RESERVED2 => {
				if executing {
					return Err(Error::DisabledOpcode(opcode));
				}
			},
			Opcode::OP_VERIF |
			Opcode::OP_VERNOTIF => {
				return Err(Error::DisabledOpcode(opcode));
			},
		}

		if stack.len() + altstack.len() > 1000 {
			return Err(Error::StackSize);
		}

		pc += instruction.step;
	}

	if !exec_stack.is_empty() {
		return Err(Error::UnbalancedConditional);
	}

	let success = !stack.is_empty() && {
		let last = try!(stack.last());
		cast_to_bool(last)
	};

	Ok(success)
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use chain::Transaction;
	use {
		Opcode, Script, VerificationFlags, Builder, Error, Num, TransactionInputSigner,
		NoopSignatureChecker, SignatureVersion, TransactionSignatureChecker, Stack
	};
	use super::{eval_script, verify_script, is_public_key};

	#[test]
	fn tests_is_public_key() {
		assert!(!is_public_key(&[]));
		assert!(!is_public_key(&[1]));
		assert!(is_public_key(&Bytes::from("0495dfb90f202c7d016ef42c65bc010cd26bb8237b06253cc4d12175097bef767ed6b1fcb3caf1ed57c98d92e6cb70278721b952e29a335134857acd4c199b9d2f")));
		assert!(is_public_key(&[2; 33]));
		assert!(is_public_key(&[3; 33]));
		assert!(!is_public_key(&[4; 33]));
	}

	// https://github.com/bitcoin/bitcoin/blob/d612837814020ae832499d18e6ee5eb919a87907/src/test/script_tests.cpp#L900
	#[test]
	fn test_push_data() {
		let expected: Stack<Bytes> = vec![vec![0x5a].into()].into();
		let flags = VerificationFlags::default()
			.verify_p2sh(true);
		let checker = NoopSignatureChecker;
		let version = SignatureVersion::Base;
		let direct: Script = vec![Opcode::OP_PUSHBYTES_1 as u8, 0x5a].into();
		let pushdata1: Script = vec![Opcode::OP_PUSHDATA1 as u8, 0x1, 0x5a].into();
		let pushdata2: Script = vec![Opcode::OP_PUSHDATA2 as u8, 0x1, 0, 0x5a].into();
		let pushdata4: Script = vec![Opcode::OP_PUSHDATA4 as u8, 0x1, 0, 0, 0, 0x5a].into();

		let mut direct_stack = Stack::new();
		let mut pushdata1_stack = Stack::new();
		let mut pushdata2_stack = Stack::new();
		let mut pushdata4_stack = Stack::new();
		assert!(eval_script(&mut direct_stack, &direct, &flags, &checker, version).unwrap());
		assert!(eval_script(&mut pushdata1_stack, &pushdata1, &flags, &checker, version).unwrap());
		assert!(eval_script(&mut pushdata2_stack, &pushdata2, &flags, &checker, version).unwrap());
		assert!(eval_script(&mut pushdata4_stack, &pushdata4, &flags, &checker, version).unwrap());

		assert_eq!(direct_stack, expected);
		assert_eq!(pushdata1_stack, expected);
		assert_eq!(pushdata2_stack, expected);
		assert_eq!(pushdata4_stack, expected);
	}

	fn basic_test(script: &Script, expected: Result<bool, Error>, expected_stack: Stack<Bytes>) {
		let flags = VerificationFlags::default()
			.verify_p2sh(true);
		let checker = NoopSignatureChecker;
		let version = SignatureVersion::Base;
		let mut stack = Stack::new();
		assert_eq!(eval_script(&mut stack, script, &flags, &checker, version), expected);
		if expected.is_ok() {
			assert_eq!(stack, expected_stack);
		}
	}

	#[test]
	fn test_equal() {
		let script = Builder::default()
			.push_data(&[0x4])
			.push_data(&[0x4])
			.push_opcode(Opcode::OP_EQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![vec![1].into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_equal_false() {
		let script = Builder::default()
			.push_data(&[0x4])
			.push_data(&[0x3])
			.push_opcode(Opcode::OP_EQUAL)
			.into_script();
		let result = Ok(false);
		let stack = vec![vec![0].into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_equal_invalid_stack() {
		let script = Builder::default()
			.push_data(&[0x4])
			.push_opcode(Opcode::OP_EQUAL)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_equal_verify() {
		let script = Builder::default()
			.push_data(&[0x4])
			.push_data(&[0x4])
			.push_opcode(Opcode::OP_EQUALVERIFY)
			.into_script();
		let result = Ok(false);
		let stack = Stack::default();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_equal_verify_failed() {
		let script = Builder::default()
			.push_data(&[0x4])
			.push_data(&[0x3])
			.push_opcode(Opcode::OP_EQUALVERIFY)
			.into_script();
		let result = Err(Error::EqualVerify);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_equal_verify_invalid_stack() {
		let script = Builder::default()
			.push_data(&[0x4])
			.push_opcode(Opcode::OP_EQUALVERIFY)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_size() {
		let script = Builder::default()
			.push_data(&[0x12, 0x34])
			.push_opcode(Opcode::OP_SIZE)
			.into_script();
		let result = Ok(true);
		let stack = vec![vec![0x12, 0x34].into(), vec![0x2].into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_size_false() {
		let script = Builder::default()
			.push_data(&[])
			.push_opcode(Opcode::OP_SIZE)
			.into_script();
		let result = Ok(false);
		let stack = vec![vec![].into(), vec![].into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_size_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_SIZE)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_hash256() {
		let script = Builder::default()
			.push_data(b"hello")
			.push_opcode(Opcode::OP_HASH256)
			.into_script();
		let result = Ok(true);
		let stack = vec!["9595c9df90075148eb06860365df33584b75bff782a510c6cd4883a419833d50".into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_hash256_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_HASH256)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_ripemd160() {
		let script = Builder::default()
			.push_data(b"hello")
			.push_opcode(Opcode::OP_RIPEMD160)
			.into_script();
		let result = Ok(true);
		let stack = vec!["108f07b8382412612c048d07d13f814118445acd".into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_ripemd160_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_RIPEMD160)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_sha1() {
		let script = Builder::default()
			.push_data(b"hello")
			.push_opcode(Opcode::OP_SHA1)
			.into_script();
		let result = Ok(true);
		let stack = vec!["aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_sha1_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_SHA1)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_sha256() {
		let script = Builder::default()
			.push_data(b"hello")
			.push_opcode(Opcode::OP_SHA256)
			.into_script();
		let result = Ok(true);
		let stack = vec!["2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_sha256_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_SHA256)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_1add() {
		let script = Builder::default()
			.push_num(5.into())
			.push_opcode(Opcode::OP_1ADD)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(6).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_1add_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_1ADD)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_1sub() {
		let script = Builder::default()
			.push_num(5.into())
			.push_opcode(Opcode::OP_1SUB)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(4).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_1sub_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_1SUB)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_negate() {
		let script = Builder::default()
			.push_num(5.into())
			.push_opcode(Opcode::OP_NEGATE)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(-5).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_negate_negative() {
		let script = Builder::default()
			.push_num((-5).into())
			.push_opcode(Opcode::OP_NEGATE)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(5).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_negate_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_NEGATE)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_abs() {
		let script = Builder::default()
			.push_num(5.into())
			.push_opcode(Opcode::OP_ABS)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(5).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_abs_negative() {
		let script = Builder::default()
			.push_num((-5).into())
			.push_opcode(Opcode::OP_ABS)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(5).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_abs_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_ABS)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_not() {
		let script = Builder::default()
			.push_num(4.into())
			.push_opcode(Opcode::OP_NOT)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_not_zero() {
		let script = Builder::default()
			.push_num(0.into())
			.push_opcode(Opcode::OP_NOT)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_not_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_NOT)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_0notequal() {
		let script = Builder::default()
			.push_num(4.into())
			.push_opcode(Opcode::OP_0NOTEQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_0notequal_zero() {
		let script = Builder::default()
			.push_num(0.into())
			.push_opcode(Opcode::OP_0NOTEQUAL)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_0notequal_invalid_stack() {
		let script = Builder::default()
			.push_opcode(Opcode::OP_0NOTEQUAL)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_add() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_ADD)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(5).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_add_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_ADD)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_sub() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_SUB)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_sub_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_SUB)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_booland() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_BOOLAND)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_booland_first() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(0.into())
			.push_opcode(Opcode::OP_BOOLAND)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_booland_second() {
		let script = Builder::default()
			.push_num(0.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_BOOLAND)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_booland_none() {
		let script = Builder::default()
			.push_num(0.into())
			.push_num(0.into())
			.push_opcode(Opcode::OP_BOOLAND)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_booland_invalid_stack() {
		let script = Builder::default()
			.push_num(0.into())
			.push_opcode(Opcode::OP_BOOLAND)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_boolor() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_BOOLOR)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_boolor_first() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(0.into())
			.push_opcode(Opcode::OP_BOOLOR)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_boolor_second() {
		let script = Builder::default()
			.push_num(0.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_BOOLOR)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_boolor_none() {
		let script = Builder::default()
			.push_num(0.into())
			.push_num(0.into())
			.push_opcode(Opcode::OP_BOOLOR)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_boolor_invalid_stack() {
		let script = Builder::default()
			.push_num(0.into())
			.push_opcode(Opcode::OP_BOOLOR)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_numequal() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_NUMEQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_numequal_not() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_NUMEQUAL)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_numequal_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_NUMEQUAL)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_numequalverify() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_NUMEQUALVERIFY)
			.into_script();
		let result = Ok(false);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_numequalverify_failed() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_NUMEQUALVERIFY)
			.into_script();
		let result = Err(Error::NumEqualVerify);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_numequalverify_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_NUMEQUALVERIFY)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_numnotequal() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_NUMNOTEQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_numnotequal_not() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_NUMNOTEQUAL)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_numnotequal_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_NUMNOTEQUAL)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_lessthan() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_LESSTHAN)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_lessthan_not() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_LESSTHAN)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_lessthan_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_LESSTHAN)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_greaterthan() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHAN)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_greaterthan_not() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHAN)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_greaterthan_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHAN)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_lessthanorequal() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_LESSTHANOREQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_lessthanorequal_equal() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_LESSTHANOREQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_lessthanorequal_not() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(1.into())
			.push_opcode(Opcode::OP_LESSTHANOREQUAL)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_lessthanorequal_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_LESSTHANOREQUAL)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_greaterthanorequal() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHANOREQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_greaterthanorequal_equal() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHANOREQUAL)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(1).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_greaterthanorequal_not() {
		let script = Builder::default()
			.push_num(1.into())
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHANOREQUAL)
			.into_script();
		let result = Ok(false);
		let stack = vec![Num::from(0).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_greaterthanorequal_invalid_stack() {
		let script = Builder::default()
			.push_num(2.into())
			.push_opcode(Opcode::OP_GREATERTHANOREQUAL)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_min() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_MIN)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(2).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_min_second() {
		let script = Builder::default()
			.push_num(4.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_MIN)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(3).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_min_invalid_stack() {
		let script = Builder::default()
			.push_num(4.into())
			.push_opcode(Opcode::OP_MIN)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_max() {
		let script = Builder::default()
			.push_num(2.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_MAX)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(3).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_max_second() {
		let script = Builder::default()
			.push_num(4.into())
			.push_num(3.into())
			.push_opcode(Opcode::OP_MAX)
			.into_script();
		let result = Ok(true);
		let stack = vec![Num::from(4).to_bytes()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_max_invalid_stack() {
		let script = Builder::default()
			.push_num(4.into())
			.push_opcode(Opcode::OP_MAX)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	#[test]
	fn test_within() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(2.into())
			.push_num(4.into())
			.push_opcode(Opcode::OP_WITHIN)
			.into_script();
		let result = Ok(true);
		let stack = vec![vec![1].into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_within_not() {
		let script = Builder::default()
			.push_num(3.into())
			.push_num(5.into())
			.push_num(4.into())
			.push_opcode(Opcode::OP_WITHIN)
			.into_script();
		let result = Ok(false);
		let stack = vec![vec![0].into()].into();
		basic_test(&script, result, stack);
	}

	#[test]
	fn test_within_invalid_stack() {
		let script = Builder::default()
			.push_num(5.into())
			.push_num(4.into())
			.push_opcode(Opcode::OP_WITHIN)
			.into_script();
		let result = Err(Error::InvalidStackOperation);
		basic_test(&script, result, Stack::default());
	}

	// https://blockchain.info/rawtx/3f285f083de7c0acabd9f106a43ec42687ab0bebe2e6f0d529db696794540fea
	#[test]
	fn test_check_transaction_signature() {
		let tx: Transaction = "0100000001484d40d45b9ea0d652fca8258ab7caa42541eb52975857f96fb50cd732c8b481000000008a47304402202cb265bf10707bf49346c3515dd3d16fc454618c58ec0a0ff448a676c54ff71302206c6624d762a1fcef4618284ead8f08678ac05b13c84235f1654e6ad168233e8201410414e301b2328f17442c0b8310d787bf3d8a404cfbd0704f135b6ad4b2d3ee751310f981926e53a6e8c39bd7d3fefd576c543cce493cbac06388f2651d1aacbfcdffffffff0162640100000000001976a914c8e90996c7c6080ee06284600c684ed904d14c5c88ac00000000".into();
		let signer: TransactionInputSigner = tx.into();
		let checker = TransactionSignatureChecker {
			signer: signer,
			input_index: 0,
		};
		let input: Script = "47304402202cb265bf10707bf49346c3515dd3d16fc454618c58ec0a0ff448a676c54ff71302206c6624d762a1fcef4618284ead8f08678ac05b13c84235f1654e6ad168233e8201410414e301b2328f17442c0b8310d787bf3d8a404cfbd0704f135b6ad4b2d3ee751310f981926e53a6e8c39bd7d3fefd576c543cce493cbac06388f2651d1aacbfcd".into();
		let output: Script = "76a914df3bd30160e6c6145baaf2c88a8844c13a00d1d588ac".into();
		let flags = VerificationFlags::default()
			.verify_p2sh(true);
		assert_eq!(verify_script(&input, &output, &flags, &checker), Ok(()));
	}

	// https://blockchain.info/rawtx/02b082113e35d5386285094c2829e7e2963fa0b5369fb7f4b79c4c90877dcd3d
	#[test]
	fn test_check_transaction_multisig() {
		let tx: Transaction = "01000000013dcd7d87904c9cb7f4b79f36b5a03f96e2e729284c09856238d5353e1182b00200000000fd5e0100483045022100deeb1f13b5927b5e32d877f3c42a4b028e2e0ce5010fdb4e7f7b5e2921c1dcd2022068631cb285e8c1be9f061d2968a18c3163b780656f30a049effee640e80d9bff01483045022100ee80e164622c64507d243bd949217d666d8b16486e153ac6a1f8e04c351b71a502203691bef46236ca2b4f5e60a82a853a33d6712d6a1e7bf9a65e575aeb7328db8c014cc9524104a882d414e478039cd5b52a92ffb13dd5e6bd4515497439dffd691a0f12af9575fa349b5694ed3155b136f09e63975a1700c9f4d4df849323dac06cf3bd6458cd41046ce31db9bdd543e72fe3039a1f1c047dab87037c36a669ff90e28da1848f640de68c2fe913d363a51154a0c62d7adea1b822d05035077418267b1a1379790187410411ffd36c70776538d079fbae117dc38effafb33304af83ce4894589747aee1ef992f63280567f52f5ba870678b4ab4ff6c8ea600bd217870a8b4f1f09f3a8e8353aeffffffff0130d90000000000001976a914569076ba39fc4ff6a2291d9ea9196d8c08f9c7ab88ac00000000".into();
		let signer: TransactionInputSigner = tx.into();
		let checker = TransactionSignatureChecker {
			signer: signer,
			input_index: 0,
		};
		let input: Script = "00483045022100deeb1f13b5927b5e32d877f3c42a4b028e2e0ce5010fdb4e7f7b5e2921c1dcd2022068631cb285e8c1be9f061d2968a18c3163b780656f30a049effee640e80d9bff01483045022100ee80e164622c64507d243bd949217d666d8b16486e153ac6a1f8e04c351b71a502203691bef46236ca2b4f5e60a82a853a33d6712d6a1e7bf9a65e575aeb7328db8c014cc9524104a882d414e478039cd5b52a92ffb13dd5e6bd4515497439dffd691a0f12af9575fa349b5694ed3155b136f09e63975a1700c9f4d4df849323dac06cf3bd6458cd41046ce31db9bdd543e72fe3039a1f1c047dab87037c36a669ff90e28da1848f640de68c2fe913d363a51154a0c62d7adea1b822d05035077418267b1a1379790187410411ffd36c70776538d079fbae117dc38effafb33304af83ce4894589747aee1ef992f63280567f52f5ba870678b4ab4ff6c8ea600bd217870a8b4f1f09f3a8e8353ae".into();
		let output: Script = "a9141a8b0026343166625c7475f01e48b5ede8c0252e87".into();
		let flags = VerificationFlags::default()
			.verify_p2sh(true);
		assert_eq!(verify_script(&input, &output, &flags, &checker), Ok(()));
	}
}