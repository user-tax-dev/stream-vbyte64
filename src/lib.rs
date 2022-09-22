#![feature(stdsimd)]

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::{ptr, slice};

pub mod tables;

pub fn keys_len(values: usize) -> usize {
  ((values + 7) / 8) * 3
}

pub fn max_compressed_len(values: usize) -> usize {
  keys_len(values) + values * 8
}

pub fn compressed_data_len(values: usize, data: &[u8]) -> usize {
  let mut len = 0;
  let keys = keys_len(values) / 3;
  for i in 0..keys {
    let mut key = 0u32;
    unsafe {
      ptr::copy_nonoverlapping(
        data.as_ptr().offset(3 * i as isize),
        &mut key as *mut u32 as *mut u8,
        3,
      );
    }
    key = u32::from_le(key);
    len += tables::LENGTH[key as usize & ((1 << 12) - 1)] as usize;
    len += tables::LENGTH[key as usize >> 12] as usize;
  }

  len - (8 - values % 8)
}

unsafe fn encode_single(value: u64, out: &mut *mut u8) -> u8 {
  let value = value.to_le();
  if value < 1 << 8 {
    **out = value as u8;
    *out = out.offset(1);
    0
  } else if value < 1 << 16 {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 2);
    *out = out.offset(2);
    1
  } else if value < 1 << 24 {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 3);
    *out = out.offset(3);
    2
  } else if value < 1 << 32 {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 4);
    *out = out.offset(4);
    3
  } else if value < 1 << 40 {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 5);
    *out = out.offset(5);
    4
  } else if value < 1 << 48 {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 6);
    *out = out.offset(6);
    5
  } else if value < 1 << 56 {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 7);
    *out = out.offset(7);
    6
  } else {
    ptr::copy_nonoverlapping(&value as *const u64 as *const u8, *out, 8);
    *out = out.offset(8);
    7
  }
}

pub unsafe fn encode_scalar(input: &[u64], keys: &mut [u8], data: &mut [u8]) -> usize {
  debug_assert!(keys.len() >= keys_len(input.len()));

  if input.is_empty() {
    return 0;
  }

  let mut keyptr = keys.as_mut_ptr();
  let mut dataptr = data.as_mut_ptr();

  let mut shift = 0;
  let mut key = 0u32;

  for &value in input {
    if shift == 24 {
      key = key.to_le();
      ptr::copy_nonoverlapping(&key as *const u32 as *const u8, keyptr, 3);
      keyptr = keyptr.offset(3);
      shift = 0;
      key = 0;
    }
    let code = encode_single(value, &mut dataptr);
    key |= (code as u32) << shift;
    shift += 3;
  }

  key = key.to_le();
  ptr::copy_nonoverlapping(&key as *const u32 as *const u8, keyptr, 3);

  let written = dataptr as usize - data.as_mut_ptr() as usize;
  debug_assert!(written <= data.len());
  written
}

#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
unsafe fn encode_block_avx(ptr: &mut *mut u8, value: __m256i) -> u32 {
  // turn each byte into a 0 or 1 based on it being nonzero
  let ones = _mm256_set1_epi8(1);
  let mins = _mm256_min_epu8(value, ones);

  // collect those bits into the high byte of each 32 bit part
  // the multiply acts like a multi-bit shift
  let low_shifter = 1 | 1 << 9 | 1 << 18;
  let high_shifter = 1 | 1 << 9 | 1 << 18 | 1 << 27;
  let shifts = _mm256_setr_epi32(
    low_shifter,
    high_shifter,
    low_shifter,
    high_shifter,
    low_shifter,
    high_shifter,
    low_shifter,
    high_shifter,
  );
  let bytemaps = _mm256_mullo_epi32(mins, shifts);

  // use that as the mask vector to select the lane code
  // first the low half
  #[cfg_attr(rustfmt, rustfmt_skip)]
    let low_lane_codes = _mm256_setr_epi8(
        0, 3, 2, 3, 1, 3, 2, 3, -1, -1, -1, -1, -1, -1, -1, -1,
        0, 3, 2, 3, 1, 3, 2, 3, -1, -1, -1, -1, -1, -1, -1, -1,
    );
  let low_shuffled_codes = _mm256_shuffle_epi8(low_lane_codes, bytemaps);
  let low_shifted_codes = _mm256_slli_epi64(low_shuffled_codes, 32);

  // now the high half
  #[cfg_attr(rustfmt, rustfmt_skip)]
    let high_lane_codes = _mm256_setr_epi8(
        0, 7, 6, 7, 5, 7, 6, 7, 4, 7, 6, 7, 5, 7, 6, 7,
        0, 7, 6, 7, 5, 7, 6, 7, 4, 7, 6, 7, 5, 7, 6, 7,
    );
  let high_shuffled_codes = _mm256_shuffle_epi8(high_lane_codes, bytemaps);

  // overlay and take the max of the low and high codes
  let lane_codes = _mm256_max_epu8(low_shifted_codes, high_shuffled_codes);

  // now gather three copies of the lane codes from each lane
  #[cfg_attr(rustfmt, rustfmt_skip)]
    let gather_high = _mm256_setr_epi8(
        -1, 15, 7, -1, -1, -1, -1, -1, -1, -1, 15, 7, -1, -1, -1, -1,
        7, -1, -1, -1, -1, 15, 7, -1, 15, 7, -1, -1, -1, -1, -1, -1,
    );
  let shuffled_codes = _mm256_shuffle_epi8(lane_codes, gather_high);
  let permuted = _mm256_permute4x64_epi64(shuffled_codes, 0b00001110);
  let high_bytes = _mm256_or_si256(shuffled_codes, permuted);

  // we're going to concatenate and sum the lane codes at the same time
  let concat_low = 1 << 8 | 1 << 19 | 1 << 30;
  let concat_high = 1 << 6 | 1 << 17;
  let sum = 1 | 1 << 8 | 1 << 16 | 1 << 24;
  let aggregators = _mm256_setr_epi32(concat_low, concat_high, sum, 0, 0, 0, 0, 0);

  let code_and_length = _mm256_mullo_epi32(high_bytes, aggregators);

  let code_low = _mm256_extract_epi8(code_and_length, 3) as u8;
  let code_high = _mm256_extract_epi8(code_and_length, 7) as u8 & 0xf;
  let code = (code_low as u32) | ((code_high as u32) << 8);
  let length = _mm256_extract_epi8(code_and_length, 11) + 4;

  let shuffle1 = tables::ENCODE_SHUFFLE_1[code as usize].v;
  let data1 = _mm256_shuffle_epi8(value, shuffle1);

  let shuffle2 = tables::ENCODE_SHUFFLE_2[code as usize].v;
  let shuffled2 = _mm256_shuffle_epi8(value, shuffle2);
  let data2 = _mm256_permute4x64_epi64(shuffled2, 0b00001110);

  let data = _mm256_or_si256(data1, data2);
  _mm256_storeu_si256(*ptr as *mut __m256i, data);
  *ptr = ptr.offset(length as isize);

  code
}

#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
pub unsafe fn encode_avx(input: &[u64], keys: &mut [u8], data: &mut [u8]) -> usize {
  debug_assert!(keys.len() >= keys_len(input.len()));

  let mut inputptr = input.as_ptr();
  let mut keyptr = keys.as_mut_ptr();
  let mut dataptr = data.as_mut_ptr();

  let count = input.len() / 8;
  for _ in 0..count {
    let data = _mm256_loadu_si256(inputptr as *const __m256i);
    inputptr = inputptr.offset(4);
    let code_low = encode_block_avx(&mut dataptr, data);

    let data = _mm256_loadu_si256(inputptr as *const __m256i);
    inputptr = inputptr.offset(4);
    let code_high = encode_block_avx(&mut dataptr, data.into());

    let code = (code_low as u32) | ((code_high as u32) << 12);
    ptr::copy_nonoverlapping(&code as *const u32 as *const u8, keyptr, 3);
    keyptr = keyptr.offset(3);
  }

  let written = dataptr as usize - data.as_ptr() as usize;
  let input = slice::from_raw_parts(inputptr, input.len() - count * 8);
  let keys = slice::from_raw_parts_mut(
    keyptr,
    keys.as_ptr().offset(keys.len() as isize) as usize - keyptr as usize,
  );
  let data = slice::from_raw_parts_mut(
    dataptr,
    data.as_ptr().offset(data.len() as isize) as usize - dataptr as usize,
  );

  encode_scalar(input, keys, data) + written
}

unsafe fn decode_single(ptr: &mut *const u8, code: u8) -> u64 {
  let mut value = 0;
  match code {
    0 => {
      value = **ptr as u64;
      *ptr = ptr.offset(1);
    }
    1 => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 2);
      value = u64::from_le(value);
      *ptr = ptr.offset(2);
    }
    2 => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 3);
      value = u64::from_le(value);
      *ptr = ptr.offset(3);
    }
    3 => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 4);
      value = u64::from_le(value);
      *ptr = ptr.offset(4);
    }
    4 => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 5);
      value = u64::from_le(value);
      *ptr = ptr.offset(5);
    }
    5 => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 6);
      value = u64::from_le(value);
      *ptr = ptr.offset(6);
    }
    6 => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 7);
      value = u64::from_le(value);
      *ptr = ptr.offset(7);
    }
    _ => {
      ptr::copy_nonoverlapping(*ptr, &mut value as *mut u64 as *mut u8, 8);
      value = u64::from_le(value);
      *ptr = ptr.offset(8);
    }
  }
  value
}

pub unsafe fn decode_scalar(output: &mut [u64], keys: &[u8], data: &[u8]) -> usize {
  debug_assert!(keys.len() >= keys_len(output.len()));

  if output.is_empty() {
    return 0;
  }

  let mut keyptr = keys.as_ptr();
  let mut dataptr = data.as_ptr();

  let mut shift = 0;
  let mut key = 0;
  ptr::copy_nonoverlapping(keyptr, &mut key as *mut u32 as *mut u8, 3);
  key = u32::from_le(key);
  keyptr = keyptr.offset(3);

  for output in output {
    if shift == 24 {
      shift = 0;
      ptr::copy_nonoverlapping(keyptr, &mut key as *mut u32 as *mut u8, 3);
      key = u32::from_le(key);
      keyptr = keyptr.offset(3);
    }
    let code = (key >> shift) & 0b111;
    *output = decode_single(&mut dataptr, code as u8);
    shift += 3;
  }

  let read = dataptr as usize - data.as_ptr() as usize;
  debug_assert!(data.len() >= read);
  read
}

#[target_feature(enable = "avx2")]
unsafe fn decode_block_avx(ptr: &mut *const u8, code: u32) -> __m256i {
  let len = tables::LENGTH[code as usize];

  let data = _mm256_loadu_si256(*ptr as *const __m256i);

  let shuffle1 = tables::DECODE_SHUFFLE_1[code as usize].v;
  let data1 = _mm256_shuffle_epi8(data, shuffle1);

  let shuffle2 = tables::DECODE_SHUFFLE_2[code as usize].v;
  let shuffled2 = _mm256_shuffle_epi8(data, shuffle2);
  let data2 = _mm256_permute4x64_epi64(shuffled2, 0b01001111);

  let data = _mm256_or_si256(data1, data2);

  *ptr = ptr.offset(len as isize);
  data.into()
}

#[target_feature(enable = "avx2")]
pub unsafe fn decode_avx(output: &mut [u64], keys: &[u8], data: &[u8]) -> usize {
  let keys_len = keys_len(output.len());
  debug_assert!(keys.len() >= keys_len);

  let mut outptr = output.as_mut_ptr();
  let mut keyptr = keys.as_ptr();
  let mut dataptr = data.as_ptr();

  // since the avx codepath loads a full 64 bytes per iteration, we need to make sure to not load
  // past the end of `data`. The worst case is if each value is 1 byte, in which case we read the
  // final 8 bytes of real data, and 56 bytes past the end. If we conservatively always take the
  // scalar path for the last 56 values, we're good.
  let block_loadable = output.len().saturating_sub(56);
  let iters = block_loadable / 8;
  for _ in 0..iters {
    let mut key = 0u32;
    ptr::copy_nonoverlapping(keyptr, &mut key as *mut u32 as *mut u8, 3);
    keyptr = keyptr.offset(3);

    debug_assert!(dataptr.offset(32) <= data.as_ptr().offset(data.len() as isize));
    let values = decode_block_avx(&mut dataptr, key & ((1 << 12) - 1));
    _mm256_storeu_si256(outptr as *mut __m256i, values);
    outptr = outptr.offset(4);

    debug_assert!(dataptr.offset(32) <= data.as_ptr().offset(data.len() as isize));
    let values = decode_block_avx(&mut dataptr, key >> 12);
    _mm256_storeu_si256(outptr as *mut __m256i, values);
    outptr = outptr.offset(4);
  }

  let read = dataptr as usize - data.as_ptr() as usize;
  let output = slice::from_raw_parts_mut(outptr, output.len() - iters * 8);
  let keys = slice::from_raw_parts(
    keyptr,
    keys.as_ptr().offset(keys.len() as isize) as usize - keyptr as usize,
  );
  let data = slice::from_raw_parts(
    dataptr,
    data.as_ptr().offset(data.len() as isize) as usize - dataptr as usize,
  );

  decode_scalar(output, keys, data) + read
}

pub fn encode(input: &[u64], buf: &mut [u8]) -> usize {
  unsafe {
    assert!(buf.len() >= max_compressed_len(input.len()));
    let keys_len = keys_len(input.len());
    let (keys, data) = buf.split_at_mut(keys_len);

    let written = {
      //if is_x86_feature_detected!("avx2") {
      #[cfg(target_feature = "avx2")]
      {
        encode_avx(input, keys, data)
      }
      //} else {
      #[cfg(not(target_feature = "avx2"))]
      {
        encode_scalar(input, keys, data)
      }
    };

    keys_len + written
  }
}

pub fn decode(output: &mut [u64], buf: &[u8]) -> usize {
  unsafe {
    let keys_len = keys_len(output.len());
    let (keys, data) = buf.split_at(keys_len);
    let data_len = compressed_data_len(output.len(), keys);
    assert!(data.len() >= data_len, "{} < {}", data.len(), data_len);

    //if is_x86_feature_detected!("avx2") {
    let n = {
      #[cfg(target_feature = "avx2")]
      {
        decode_avx(output, keys, data)
      }

      #[cfg(not(target_feature = "avx2"))]
      {
        decode_scalar(output, keys, data)
      }
    };
    n
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn check_compressed_len() {
    let values = (0..4090)
      .map(|v| v * (u64::max_value() / 4090))
      .collect::<Vec<_>>();
    let len = max_compressed_len(values.len());
    let mut buf = vec![0; len];
    let written = encode(&values, &mut buf);
    let data_len = compressed_data_len(values.len(), &buf);
    assert_eq!(written, keys_len(values.len()) + data_len);
  }

  #[test]
  fn base_round_trip() {
    let values = (0..4090)
      .map(|v| v * (u64::max_value() / 4090))
      .collect::<Vec<_>>();
    let len = max_compressed_len(values.len());
    let mut buf = vec![0; len];
    let written = encode(&values, &mut buf);
    let mut out = vec![0; values.len()];
    decode(&mut out, &buf[..written]);
    assert_eq!(values, out);
  }

  #[test]
  fn short_round_trip() {
    let values = [0, 1, 2, 3, 4, 5, 6, 7];
    let len = max_compressed_len(values.len());
    let mut buf = vec![0; len];
    let written = encode(&values, &mut buf);
    let mut out = [0; 8];
    decode(&mut out, &buf[..written]);
    assert_eq!(values, out);
  }

  #[test]
  fn scalar_round_trip() {
    unsafe {
      let values = (0..4090)
        .map(|v| v * (u64::max_value() / 4090))
        .collect::<Vec<_>>();
      let mut keys = vec![0; keys_len(values.len())];
      let mut data = vec![0; values.len() * 8];

      let written = encode_scalar(&values, &mut keys, &mut data);
      let mut out = vec![0; values.len()];
      let read = decode_scalar(&mut out, &keys, &data);
      assert_eq!(read, written);
      assert_eq!(values, out);
    }
  }

  #[cfg(target_feature = "avx2")]
  #[test]
  fn match_encode() {
    unsafe {
      let values = [
        5,
        5 << 8 | 2,
        5 << 16 | 2,
        5 << 24 | 2,
        5 << 32 | 2,
        5 << 40 | 2,
        5 << 48 | 2,
        5 << 56 | 2,
      ];
      let mut keys1 = vec![0; keys_len(values.len())];
      let mut data1 = vec![0; values.len() * 8];
      let written1 = encode_scalar(&values, &mut keys1, &mut data1);

      let mut keys2 = vec![0; keys_len(values.len())];
      let mut data2 = vec![0; values.len() * 8];
      let written2 = encode_avx(&values, &mut keys2, &mut data2);

      assert_eq!(keys1, keys2);
      assert_eq!(data1, data2);
      assert_eq!(written1, written2);
    }
  }

  #[test]
  fn single_round_trip() {
    let tests = [
      0,
      5,
      5 << 8 | 2,
      5 << 16 | 2,
      5 << 24 | 2,
      5 << 32 | 2,
      5 << 40 | 2,
      5 << 48 | 2,
      5 << 56 | 2,
    ];
    for &test in &tests {
      unsafe {
        let mut buf = [0; 8];
        let mut write_ptr = buf.as_mut_ptr();
        let code = encode_single(test, &mut write_ptr);
        let mut read_ptr = buf.as_ptr();
        let out = decode_single(&mut read_ptr, code);
        assert_eq!(write_ptr as *const u8, read_ptr);
        assert_eq!(test, out);
      }
    }
  }
}
