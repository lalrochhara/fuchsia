// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Encoding contains functions and traits for FIDL encoding and decoding.

use {
    crate::handle::{
        invoke_for_handle_types, Handle, HandleBased, HandleDisposition, HandleInfo, HandleOp,
        ObjectType, Rights, Status,
    },
    crate::{Error, Result},
    bitflags::bitflags,
    fuchsia_zircon_status as zx_status,
    static_assertions::{assert_not_impl_any, assert_obj_safe},
    std::{cell::RefCell, cell::RefMut, cmp, mem, ptr, str, u32, u64},
};

struct TlsBuf {
    bytes: Vec<u8>,
    encode_handles: Vec<HandleDisposition<'static>>,
    decode_handles: Vec<HandleInfo>,
}

impl TlsBuf {
    fn new() -> TlsBuf {
        TlsBuf { bytes: Vec::new(), encode_handles: Vec::new(), decode_handles: Vec::new() }
    }
}

thread_local!(static TLS_BUF: RefCell<TlsBuf> = RefCell::new(TlsBuf::new()));

const MIN_TLS_BUF_BYTES_SIZE: usize = 512;

/// Acquire a mutable reference to the thread-local buffers used for encoding.
///
/// This function may not be called recursively.
#[inline]
pub fn with_tls_encode_buf<R>(
    f: impl FnOnce(&mut Vec<u8>, &mut Vec<HandleDisposition<'static>>) -> R,
) -> R {
    TLS_BUF.with(|buf| {
        let (mut bytes, mut handles) =
            RefMut::map_split(buf.borrow_mut(), |b| (&mut b.bytes, &mut b.encode_handles));
        if bytes.capacity() == 0 {
            bytes.reserve(MIN_TLS_BUF_BYTES_SIZE);
        }
        let res = f(&mut bytes, &mut handles);
        bytes.clear();
        handles.clear();
        res
    })
}

/// Acquire a mutable reference to the thread-local buffers used for decoding.
///
/// This function may not be called recursively.
#[inline]
pub fn with_tls_decode_buf<R>(f: impl FnOnce(&mut Vec<u8>, &mut Vec<HandleInfo>) -> R) -> R {
    TLS_BUF.with(|buf| {
        let (mut bytes, mut handles) =
            RefMut::map_split(buf.borrow_mut(), |b| (&mut b.bytes, &mut b.decode_handles));
        if bytes.capacity() == 0 {
            bytes.reserve(MIN_TLS_BUF_BYTES_SIZE);
        }
        let res = f(&mut bytes, &mut handles);
        bytes.clear();
        handles.clear();
        res
    })
}

/// Encodes the provided type into the thread-local encoding buffers.
///
/// This function may not be called recursively.
#[inline]
pub fn with_tls_encoded<T, E: From<Error>>(
    val: &mut impl Encodable,
    f: impl FnOnce(&mut Vec<u8>, &mut Vec<HandleDisposition<'static>>) -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    with_tls_encode_buf(|bytes, handles| {
        Encoder::encode(bytes, handles, val)?;
        f(bytes, handles)
    })
}

/// Resize a vector without zeroing added bytes.
///
/// The type `T` must be `Copy`. This is not enforced in the type signature
/// because it is used in generic contexts where verifying this requires looking
/// at control flow. See `decode_vector` for an example.
///
/// # Safety
///
/// This is unsafe when `new_len > old_len` because it leaves new elements at
/// indices `old_len..new_len` unintialized. The caller must overwrite all the
/// new elements before reading them. "Reading" includes any operation that
/// extends the vector, such as `push`, because this could reallocate the vector
/// and copy the uninitialized bytes.
///
/// FIDL conformance tests are used to validate that there are no uninitialized
/// bytes in the output across a range types and values.
#[inline]
unsafe fn resize_vec_no_zeroing<T>(buf: &mut Vec<T>, new_len: usize) {
    if new_len > buf.capacity() {
        buf.reserve(new_len - buf.len());
    }
    // Safety:
    // - `new_len` must be less than or equal to `capacity()`:
    //   The if-statement above guarantees this.
    // - The elements at `old_len..new_len` must be initialized:
    //   They are purposely left uninitialized, making this function unsafe.
    buf.set_len(new_len);
}

/// Rounds `x` up if necessary so that it is a multiple of `align`.
///
/// Requires `align` to be a (nonzero) power of two.
#[inline(always)]
pub fn round_up_to_align(x: usize, align: usize) -> usize {
    debug_assert_ne!(align, 0);
    debug_assert_eq!(align & (align - 1), 0);
    // https://en.wikipedia.org/wiki/Data_structure_alignment#Computing_padding
    (x + align - 1) & !(align - 1)
}

#[doc(hidden)] // only exported for macro use
#[inline]
pub fn take_handle<T: HandleBased>(handle: &mut T) -> Handle {
    let invalid = T::from_handle(Handle::invalid());
    mem::replace(handle, invalid).into_handle()
}

/// The maximum recursion depth of encoding and decoding.
/// Each nested aggregate type (structs, unions, arrays, or vectors) counts as one step in the
/// recursion depth.
pub const MAX_RECURSION: usize = 32;

/// Indicates that an optional value is present.
pub const ALLOC_PRESENT_U64: u64 = u64::MAX;
/// Indicates that an optional value is present.
pub const ALLOC_PRESENT_U32: u32 = u32::MAX;
/// Indicates that an optional value is absent.
pub const ALLOC_ABSENT_U64: u64 = 0;
/// Indicates that an optional value is absent.
pub const ALLOC_ABSENT_U32: u32 = 0;

/// Special ordinal signifying an epitaph message.
pub const EPITAPH_ORDINAL: u64 = 0xffffffffffffffffu64;

/// The current wire format magic number
pub const MAGIC_NUMBER_INITIAL: u8 = 1;

/// Context for encoding and decoding.
///
/// This is currently empty. We keep it around to ease the implementation of
/// context-dependent behavior for future migrations.
///
/// WARNING: Do not construct this directly unless you know what you're doing.
/// FIDL uses `Context` to coordinate soft migrations, so improper uses of it
/// could result in ABI breakage.
#[derive(Clone, Copy, Debug)]
pub struct Context {}

impl Context {
    /// Returns the header flags to set when encoding with this context.
    #[inline]
    fn header_flags(&self) -> HeaderFlags {
        HeaderFlags::empty()
    }
}

/// Encoding state
#[derive(Debug)]
pub struct Encoder<'a, 'b> {
    /// Buffer to write output data into.
    ///
    /// New chunks of out-of-line data should be appended to the end of the `Vec`.
    /// `buf` should be resized to be large enough for any new data *prior* to encoding the inline
    /// portion of that data.
    buf: &'a mut Vec<u8>,

    /// Buffer to write output handles into.
    handles: &'a mut Vec<HandleDisposition<'b>>,

    /// Encoding context.
    context: &'a Context,

    /// Handle subtype for the next handle that is encoded.
    next_handle_subtype: ObjectType,

    /// Handle rights for the next handle that is encoded.
    next_handle_rights: Rights,
}

/// The default context for encoding.
/// During migrations, this controls the default write path.
#[inline]
fn default_encode_context() -> Context {
    Context {}
}

impl<'a, 'b> Encoder<'a, 'b> {
    /// FIDL-encodes `x` into the provided data and handle buffers.
    #[inline]
    pub fn encode<T: Encodable + ?Sized>(
        buf: &'a mut Vec<u8>,
        handles: &'a mut Vec<HandleDisposition<'b>>,
        x: &mut T,
    ) -> Result<()> {
        let context = default_encode_context();
        Self::encode_with_context(&context, buf, handles, x)
    }

    /// FIDL-encodes `x` into the provided data and handle buffers, using the
    /// specified encoding context.
    ///
    /// WARNING: Do not call this directly unless you know what you're doing.
    /// FIDL uses `Context` to coordinate soft migrations, so improper uses of
    /// this function could result in ABI breakage.
    #[inline]
    pub fn encode_with_context<T: Encodable + ?Sized>(
        context: &Context,
        buf: &'a mut Vec<u8>,
        handles: &'a mut Vec<HandleDisposition<'b>>,
        x: &mut T,
    ) -> Result<()> {
        fn prepare_for_encoding<'a, 'b>(
            context: &'a Context,
            buf: &'a mut Vec<u8>,
            handles: &'a mut Vec<HandleDisposition<'b>>,
            ty_inline_size: usize,
        ) -> Encoder<'a, 'b> {
            // An empty response can have size zero.
            // This if statement is needed to not break the padding write below.
            if ty_inline_size != 0 {
                let aligned_inline_size = round_up_to_align(ty_inline_size, 8);
                // Safety: The uninitialized elements are written by `x.encode`,
                // except for the trailing padding which is zeroed below.
                unsafe {
                    resize_vec_no_zeroing(buf, aligned_inline_size);

                    // Zero the last 8 bytes in the block to ensure padding bytes are zero.
                    let padding_ptr = buf.get_unchecked_mut(aligned_inline_size - 8);
                    mem::transmute::<*mut u8, *mut u64>(padding_ptr).write_unaligned(0);
                }
            }
            handles.truncate(0);
            Encoder {
                buf,
                handles,
                context,
                next_handle_subtype: ObjectType::NONE,
                next_handle_rights: Rights::NONE,
            }
        }
        let mut encoder = prepare_for_encoding(context, buf, handles, x.inline_size(context));
        x.encode(&mut encoder, 0, 0)
    }

    /// Returns the inline alignment of an object of type `Target` for this encoder.
    #[inline(always)]
    pub fn inline_align_of<Target: Encodable>(&self) -> usize {
        <Target as Layout>::inline_align(&self.context)
    }

    /// Returns the inline size of the given object for this encoder.
    #[inline(always)]
    pub fn inline_size_of<Target: Encodable>(&self) -> usize {
        <Target as Layout>::inline_size(&self.context)
    }

    /// In debug mode only, asserts that there is enough room in the buffer to
    /// write an object of type `Target` at `offset`.
    #[inline(always)]
    pub fn debug_check_bounds<Target: Encodable>(&self, offset: usize) {
        debug_assert!(offset + self.inline_size_of::<Target>() <= self.buf.len());
    }

    /// Extends buf by `len` bytes and calls the provided closure to write
    /// out-of-line data, with `offset` set to the start of the new region.
    /// `len` must be nonzero.
    #[inline(always)]
    pub fn write_out_of_line<F>(&mut self, len: usize, recursion_depth: usize, f: F) -> Result<()>
    where
        F: FnOnce(&mut Encoder<'_, '_>, usize, usize) -> Result<()>,
    {
        debug_assert!(len > 0);
        let new_offset = self.buf.len();
        let new_depth = recursion_depth + 1;
        Self::check_recursion_depth(new_depth)?;
        let padded_len = round_up_to_align(len, 8);
        // Safety: The uninitialized elements are written by `f`, except the
        // trailing padding which is zeroed below.
        unsafe {
            // In order to zero bytes for padding, we assume that at least 8 bytes are in the
            // out-of-line block.
            debug_assert!(padded_len >= 8);

            let new_len = self.buf.len() + padded_len;
            resize_vec_no_zeroing(self.buf, new_len);

            // Zero the last 8 bytes in the block to ensure padding bytes are zero.
            let padding_ptr = self.buf.get_unchecked_mut(new_len - 8);
            mem::transmute::<*mut u8, *mut u64>(padding_ptr).write_unaligned(0);
        }
        f(self, new_offset, new_depth)
    }

    /// Validate that the recursion depth is within the limit.
    #[inline(always)]
    pub fn check_recursion_depth(recursion_depth: usize) -> Result<()> {
        if recursion_depth > MAX_RECURSION {
            return Err(Error::MaxRecursionDepth);
        }
        Ok(())
    }

    /// Append bytes to the very end (out-of-line) of the buffer.
    #[inline]
    pub fn append_out_of_line_bytes(&mut self, bytes: &[u8]) {
        if bytes.len() == 0 {
            return;
        }

        let start = self.buf.len();
        let end = self.buf.len() + round_up_to_align(bytes.len(), 8);

        // Safety:
        // - self.buf is initially uninitialized when resized, but it is then
        //   initialized by a later copy so it leaves this block initialized.
        // - There is enough room for the 8 byte padding filler because end's
        //   alignment is rounded up to 8 bytes and bytes.len() != 0.
        unsafe {
            resize_vec_no_zeroing(self.buf, end);

            let padding_ptr = self.buf.get_unchecked_mut(end - 8);
            mem::transmute::<*mut u8, *mut u64>(padding_ptr).write_unaligned(0);

            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.buf.as_mut_ptr().offset(start as isize),
                bytes.len(),
            );
        }
    }

    /// Append handles to the buffer.
    #[inline]
    pub fn append_unknown_handles(&mut self, handles: &mut [Handle]) {
        self.handles.reserve(handles.len());
        for handle in handles {
            // Unknown handles don't have object type and rights.
            self.handles.push(HandleDisposition {
                handle_op: HandleOp::Move(take_handle(handle)),
                object_type: ObjectType::NONE,
                rights: Rights::SAME_RIGHTS,
                result: Status::OK,
            });
        }
    }

    /// Returns the encoder's context.
    ///
    /// This is needed for accessing the context in macros during migrations.
    #[inline]
    pub fn context(&self) -> &Context {
        self.context
    }

    /// Write padding at the specified offset.
    #[inline(always)]
    pub fn padding(&mut self, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        // In practice, this assertion should never fail because we ensure that
        // padding is within an already allocated block outside of this
        // function.
        assert!(offset + len <= self.buf.len());
        // Safety:
        // - The pointer is valid for this range, as tested by the assertion above.
        // - All u8 pointers are properly aligned.
        unsafe {
            ptr::write_bytes(self.buf.as_mut_ptr().offset(offset as isize), 0, len);
        }
    }

    /// Acquires a mutable reference to the buffer in the encoder.
    #[inline]
    pub fn mut_buffer(&mut self) -> &mut [u8] {
        self.buf
    }

    /// Sets the handle subtype of the next handle.
    #[inline]
    pub fn set_next_handle_subtype(&mut self, object_type: ObjectType) {
        self.next_handle_subtype = object_type
    }

    /// Sets the handle rights of the next handle.
    #[inline]
    pub fn set_next_handle_rights(&mut self, rights: Rights) {
        self.next_handle_rights = rights
    }
}

/// Decoding state
#[derive(Debug)]
pub struct Decoder<'a> {
    /// The out of line depth.
    depth: usize,

    /// Next out of line block in buf.
    next_out_of_line: usize,

    /// Buffer from which to read data.
    buf: &'a [u8],

    /// Buffer from which to read handles.
    handles: &'a mut [HandleInfo],

    /// Index of the next handle to read from the handle array
    next_handle: usize,

    /// Decoding context.
    context: &'a Context,

    /// Handle subtype for the next handle that is decoded.
    next_handle_subtype: ObjectType,

    /// Handle rights for the next handle that is decoded.
    next_handle_rights: Rights,
}

impl<'a> Decoder<'a> {
    /// FIDL-decodes a value of type `T` from the provided data and handle
    /// buffers. Assumes the buffers came from inside a transaction message
    /// wrapped by `header`.
    #[inline]
    pub fn decode_into<T: Decodable>(
        header: &TransactionHeader,
        buf: &'a [u8],
        handles: &'a mut [HandleInfo],
        value: &mut T,
    ) -> Result<()> {
        Self::decode_with_context(&header.decoding_context(), buf, handles, value)
    }

    /// Checks for errors after decoding. This is a separate function to reduce binary bloat.
    /// Requires that `padding_end <= self.buf.len()`.
    fn post_decoding(&self, padding_start: usize, padding_end: usize) -> Result<()> {
        if self.next_out_of_line < self.buf.len() {
            return Err(Error::ExtraBytes);
        }
        if self.next_handle < self.handles.len() {
            return Err(Error::ExtraHandles);
        }

        let padding = padding_end - padding_start;
        if padding > 0 {
            // Safety:
            // padding_end <= self.buf.len() is guaranteed by the caller.
            let last_u64 = unsafe {
                let last_u64_ptr = self.buf.get_unchecked(padding_end - 8);
                mem::transmute::<*const u8, *const u64>(last_u64_ptr).read_unaligned()
            };
            // padding == 0 => mask == 0x0000000000000000
            // padding == 1 => mask == 0xff00000000000000
            // padding == 2 => mask == 0xffff000000000000
            // ...
            let mask = !(!0u64 >> padding * 8);
            if last_u64 & mask != 0 {
                return Err(self.end_of_block_padding_error(padding_start, padding_end));
            }
        }

        Ok(())
    }

    /// FIDL-decodes a value of type `T` from the provided data and handle
    /// buffers, using the specified context.
    ///
    /// WARNING: Do not call this directly unless you know what you're doing.
    /// FIDL uses `Context` to coordinate soft migrations, so improper uses of
    /// this function could result in ABI breakage.
    #[inline]
    pub fn decode_with_context<T: Decodable>(
        context: &Context,
        buf: &'a [u8],
        handles: &'a mut [HandleInfo],
        value: &mut T,
    ) -> Result<()> {
        let inline_size = <T as Layout>::inline_size(context);
        let next_out_of_line = round_up_to_align(inline_size, 8);
        if next_out_of_line > buf.len() {
            return Err(Error::OutOfRange);
        }
        let mut decoder = Decoder {
            depth: 0,
            next_out_of_line: next_out_of_line,
            buf: buf,
            handles: handles,
            next_handle: 0,
            context: context,
            next_handle_subtype: ObjectType::NONE,
            next_handle_rights: Rights::NONE,
        };
        value.decode(&mut decoder, 0)?;
        // Safety: next_out_of_line <= buf.len() based on the if-statement above.
        decoder.post_decoding(inline_size, next_out_of_line)
    }

    fn consume_handle_info(&self, mut handle_info: HandleInfo) -> Result<Handle> {
        let received_subtype = handle_info.object_type;
        let expected_subtype = self.next_handle_subtype;
        if expected_subtype != ObjectType::NONE
            && received_subtype != ObjectType::NONE
            && expected_subtype != received_subtype
        {
            return Err(Error::IncorrectHandleSubtype {
                expected: expected_subtype,
                received: received_subtype,
            });
        }

        let received_rights = handle_info.rights;
        let expected_rights = self.next_handle_rights;
        // We shouldn't receive a handle with no rights.
        // At a minimum TRANSFER or DUPLICATE is needed to send the handle.
        assert!(!expected_rights.is_empty());
        if expected_rights != Rights::SAME_RIGHTS
            && received_rights != Rights::SAME_RIGHTS
            && expected_rights != received_rights
        {
            if !received_rights.contains(expected_rights) {
                return Err(Error::MissingExpectedHandleRights {
                    missing_rights: expected_rights - received_rights,
                });
            }
            return match handle_info.handle.replace(expected_rights) {
                Ok(r) => Ok(r),
                Err(status) => Err(Error::HandleReplace(status)),
            };
        }
        Ok(mem::replace(&mut handle_info.handle, Handle::invalid()))
    }

    /// Take the next handle from the `handles` list.
    #[inline]
    pub fn take_next_handle(&mut self) -> Result<Handle> {
        if self.next_handle >= self.handles.len() {
            return Err(Error::OutOfRange);
        }
        let handle_info = mem::replace(
            &mut self.handles[self.next_handle],
            HandleInfo {
                handle: Handle::invalid(),
                object_type: ObjectType::NONE,
                rights: Rights::NONE,
            },
        );
        let handle = self.consume_handle_info(handle_info)?;
        self.next_handle += 1;
        Ok(handle)
    }

    /// Drops the next handle in the handle array.
    #[inline]
    pub fn drop_next_handle(&mut self) -> Result<()> {
        if self.next_handle >= self.handles.len() {
            return Err(Error::OutOfRange);
        }
        drop(mem::replace(
            &mut self.handles[self.next_handle],
            HandleInfo {
                handle: Handle::invalid(),
                object_type: ObjectType::NONE,
                rights: Rights::NONE,
            },
        ));
        self.next_handle += 1;
        Ok(())
    }

    /// Generates an error for bad padding bytes at the end of a block.
    /// Assumes that it is already known that there is an error.
    fn end_of_block_padding_error(&self, start: usize, end: usize) -> Error {
        for i in start..end {
            if self.buf[i] != 0 {
                return Error::NonZeroPadding { padding_start: start, non_zero_pos: i };
            }
        }
        panic!("invalid padding bytes detected, but missing when generating error");
    }

    /// Runs the provided closure inside an decoder modified to read out-of-line data.
    #[inline(always)]
    pub fn read_out_of_line<F, R>(&mut self, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut Decoder<'_>, usize) -> Result<R>,
    {
        // Compute offsets for out of line block.
        let offset = self.next_out_of_line;
        let aligned_len = round_up_to_align(len, 8);
        self.next_out_of_line = self.next_out_of_line + aligned_len;
        if self.next_out_of_line > self.buf.len() {
            return Err(Error::OutOfRange);
        }

        // Validate padding bytes at the end of the block.
        // Safety:
        // - self.next_out_of_line <= self.buf.len() based on the if-statement above.
        // - If `len` is 0, `next_out_of_line` is unchanged and this will read
        //   the prior 8 bytes. This is valid because at least 8 inline bytes
        //   are always read before calling `read_out_of_line`. The `mask` will
        //   be zero so the check will not fail.
        debug_assert!(self.next_out_of_line >= 8);
        let last_u64 = unsafe {
            let last_u64_ptr = self.buf.get_unchecked(self.next_out_of_line - 8);
            mem::transmute::<*const u8, *const u64>(last_u64_ptr).read_unaligned()
        };
        let padding = aligned_len - len;
        // padding == 0 => mask == 0x0000000000000000
        // padding == 1 => mask == 0xff00000000000000
        // padding == 2 => mask == 0xffff000000000000
        // ...
        let mask = !(!0u64 >> padding * 8);
        if last_u64 & mask != 0 {
            return Err(self.end_of_block_padding_error(offset + len, self.next_out_of_line));
        }

        // Descend into block.
        self.depth += 1;
        if self.depth > MAX_RECURSION {
            return Err(Error::MaxRecursionDepth);
        }
        let res = f(self, offset)?;
        self.depth -= 1;

        // Return.
        Ok(res)
    }

    /// The number of handles that have not yet been consumed.
    #[inline]
    pub fn remaining_handles(&self) -> usize {
        self.handles.len() - self.next_handle
    }

    /// A convenience method to skip over the specified number of zero bytes used for padding, also
    /// checking that all those bytes are in fact zeroes.
    #[inline]
    pub fn check_padding(&self, offset: usize, len: usize) -> Result<()> {
        if len == 0 {
            // Skip body (so it can be optimized out).
            return Ok(());
        }
        for i in offset..offset + len {
            if self.buf[i] != 0 {
                return Err(Error::NonZeroPadding { padding_start: offset, non_zero_pos: i });
            }
        }
        Ok(())
    }

    /// Returns the inline alignment of an object of type `Target` for this decoder.
    #[inline(always)]
    pub fn inline_align_of<Target: Decodable>(&self) -> usize {
        <Target as Layout>::inline_align(&self.context)
    }

    /// Returns the inline size of an object of type `Target` for this decoder.
    #[inline(always)]
    pub fn inline_size_of<Target: Decodable>(&self) -> usize {
        <Target as Layout>::inline_size(&self.context)
    }

    /// In debug mode only, asserts that there is enough room in the buffer to
    /// read an object of type `Target` at `offset`.
    #[inline(always)]
    pub fn debug_check_bounds<Target: Decodable>(&self, offset: usize) {
        debug_assert!(offset + self.inline_size_of::<Target>() <= self.buf.len());
    }

    /// Returns the decoder's context.
    ///
    /// This is needed for accessing the context in macros during migrations.
    #[inline]
    pub fn context(&self) -> &Context {
        self.context
    }

    /// The position of the next out of line block and the end of the current
    /// blocks.
    #[inline]
    pub fn next_out_of_line(&self) -> usize {
        self.next_out_of_line
    }

    /// The buffer holding the message to be decoded.
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        self.buf
    }

    /// Sets the handle subtype of the next handle.
    #[inline]
    pub fn set_next_handle_subtype(&mut self, object_type: ObjectType) {
        self.next_handle_subtype = object_type
    }

    /// Sets the handle rights of the next handle.
    #[inline]
    pub fn set_next_handle_rights(&mut self, rights: Rights) {
        self.next_handle_rights = rights
    }
}

/// A trait for specifying the inline layout of an encoded object.
pub trait Layout {
    /// Returns the minimum required alignment of the inline portion of the
    /// encoded object. It must be a (nonzero) power of two.
    fn inline_align(context: &Context) -> usize
    where
        Self: Sized;

    /// Returns the size of the inline portion of the encoded object, including
    /// padding for the type's alignment. Must be a multiple of `inline_align`.
    fn inline_size(context: &Context) -> usize
    where
        Self: Sized;

    /// Returns true iff the type can be encoded or decoded via simple copy.
    ///
    /// Implementations that return true must use `mem::align_of::<Self>()` and
    /// `mem::size_of::<Self>()` for `inline_align` and `inline_size`.
    /// TODO: enforce this.
    ///
    /// Simple copying only works for types when (1) the Rust data layout
    /// matches the FIDL wire format, and (2) no validation is required. This is
    /// the case for (arrays of) primitive integer types, because both FIDL and
    /// Rust (we assume) use little-endian byte order, two's complement integer
    /// representation, and arrays with no padding.
    ///
    /// For more information:
    /// https://doc.rust-lang.org/reference/type-layout.html#primitive-data-layout
    /// https://doc.rust-lang.org/reference/types/numeric.html
    #[inline(always)]
    fn supports_simple_copy() -> bool
    where
        Self: Sized,
    {
        false
    }
}

/// An object-safe extension of the `Layout` trait.
///
/// This trait should not be implemented directly. Instead, types should
/// implement `Layout` and rely on the automatic implementation of this one.
///
/// The purpose of this trait is to provide access to inline size and alignment
/// values through `dyn Encodable` trait objects, including generic contexts
/// where they are allowed such as `T: Encodable + ?Sized`.
pub trait LayoutObject: Layout {
    /// See `Layout::inline_align`.
    fn inline_align(&self, context: &Context) -> usize;

    /// See `Layout::inline_size`.
    fn inline_size(&self, context: &Context) -> usize;
}

assert_obj_safe!(LayoutObject);

impl<T: Layout> LayoutObject for T {
    #[inline(always)]
    fn inline_align(&self, context: &Context) -> usize {
        <T as Layout>::inline_align(context)
    }

    #[inline(always)]
    fn inline_size(&self, context: &Context) -> usize {
        <T as Layout>::inline_size(context)
    }
}

/// A type which can be FIDL-encoded into a buffer.
///
/// Often an `Encodable` type should also be `Decodable`, but this is not always
/// the case. For example, both `String` and `&str` are encodable, but `&str` is
/// not decodable since it does not own any memory to store the string.
///
/// This trait is object-safe, meaning it is possible to create `dyn Encodable`
/// trait objects. Using them instead of generic `T: Encodable` types can help
/// reduce binary bloat. However, they can only be encoded directly: there are
/// no implementations of `Encodable` for enclosing types such as
/// `Vec<&dyn Encodable>`, and similarly for references, arrays, tuples, etc.
pub trait Encodable: LayoutObject {
    /// Encode the object into the buffer. Any handles stored in the object are
    /// swapped for `Handle::INVALID`. Callers must ensure that `offset` is a
    /// multiple of `Layout::inline_align`, and that `encoder.buf` has room for
    /// writing `Layout::inline_size` bytes at `offset`.
    ///
    /// Implementations must write every byte in `encoder.buf[offset..offset+S]`
    /// (where `S` is `Layout::inline_size`) unless returning an `Err` value.
    /// Implementations that encode out-of-line objects must pass
    /// `recursion_depth` to `Encoder::write_out_of_line`, or manually call
    /// `Encoder::check_recursion_depth(recursion_depth + 1)`.
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()>;
}

assert_obj_safe!(Encodable);

/// A type which can be FIDL-decoded from a buffer.
///
/// This trait is not object-safe, since `new_empty` returns `Self`. This is not
/// really a problem: there are not many use cases for `dyn Decodable`.
pub trait Decodable: Layout + Sized {
    /// Creates a new value of this type with an "empty" representation.
    fn new_empty() -> Self;

    /// Decodes an object of this type from the decoder's buffers into `self`.
    /// Callers must ensure that `offset` is a multiple of
    /// `Layout::inline_align`, and that `decoder.buf` has room for reading
    /// `Layout::inline_size` bytes at `offset`.
    ///
    /// Implementations must read every byte in `decoder.buf[offset..offset+S]`
    /// (where `S` is `Layout::inline_size`) and validate them, unless returning
    /// an `Err` value.
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()>;
}

macro_rules! impl_layout {
    ($ty:ty, align: $align:expr, size: $size:expr) => {
        impl Layout for $ty {
            #[inline(always)]
            fn inline_size(_context: &Context) -> usize {
                $size
            }
            #[inline(always)]
            fn inline_align(_context: &Context) -> usize {
                $align
            }
        }
    };
}

macro_rules! impl_layout_forall_T {
    ($ty:ty, align: $align:expr, size: $size:expr) => {
        impl<T: Layout> Layout for $ty {
            #[inline(always)]
            fn inline_size(_context: &Context) -> usize {
                $size
            }
            #[inline(always)]
            fn inline_align(_context: &Context) -> usize {
                $align
            }
        }
    };
}

// Implements `Layout` for a primitive integer type, overriding `supports_simple_copy`
// to enable encoding and decoding by simple copy.
macro_rules! impl_layout_int {
    ($int_ty:ty) => {
        impl Layout for $int_ty {
            #[inline(always)]
            fn inline_size(_context: &Context) -> usize {
                mem::size_of::<$int_ty>()
            }

            #[inline(always)]
            fn inline_align(_context: &Context) -> usize {
                mem::size_of::<$int_ty>()
            }

            #[inline(always)]
            fn supports_simple_copy() -> bool {
                true
            }
        }
    };
}

// This macro implements Encodable and Decodable for primitive integer types T.
// It ensures that T, [T; N], and Vec<T> are encoded/decoded by simple copy (via
// impl_layout_int), and that &[T] is encoded by simple copy (via
// impl_slice_encoding_by_copy).
//
// Some background on why we have the &[T] implementation: the FIDL type
// vector<T> becomes &mut dyn ExactSizeIterator<Item = T> (borrowed) or Vec<T>
// (owned) for most types. The former is a poor fit for vectors of primitives:
// we cannot optimize encoding from an iterator. For this reason, vectors of
// primitives are special-cased in fidlgen to use &[T] as the borrowed type.
//
// Caveat: bool uses &mut dyn ExactSizeIterator<Item = bool> because it cannot
// be optimized. Floats f32 and f64, though they cannot be optimized either, use
// &[f32] and &[f64].
// TODO(fxbug.dev/54368): Resolve this inconsistency.
macro_rules! impl_codable_int { ($($int_ty:ty,)*) => { $(
    impl_layout_int!($int_ty);

    impl Encodable for $int_ty {
        #[inline(always)]
        fn encode(&mut self, encoder: &mut Encoder<'_, '_>, offset: usize, _recursion_depth: usize) -> Result<()> {
            encoder.debug_check_bounds::<Self>(offset);
            // Safety: The caller ensures `offset` is valid for writing
            // sizeof($int_ty) bytes. Transmuting to a same-or-wider integer is
            // safe because we use `write_unaligned`.
            unsafe {
                let ptr = encoder.buf.get_unchecked_mut(offset);
                let int_ptr = mem::transmute::<*mut u8, *mut $int_ty>(ptr);
                int_ptr.write_unaligned(*self);
            }
            Ok(())
        }
    }

    impl Decodable for $int_ty {
        #[inline(always)]
        fn new_empty() -> Self { 0 as $int_ty }

        #[inline(always)]
        fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
            decoder.debug_check_bounds::<Self>(offset);
            // Safety: The caller ensures `offset` is valid for reading
            // sizeof($int_ty) bytes. Transmuting to a same-or-wider integer is
            // safe because we use `read_unaligned`.
            unsafe {
                let ptr = decoder.buf.get_unchecked(offset);
                let int_ptr = mem::transmute::<*const u8, *const $int_ty>(ptr);
                *self = int_ptr.read_unaligned();
            }
            Ok(())
        }
    }

    impl_slice_encoding_by_copy!($int_ty);
)* } }

// This is separate from impl_codable_int because floats will require validation
// in the future (FTP-055), so we can't encode/decode by simple copy.
macro_rules! impl_codable_float { ($($float_ty:ty,)*) => { $(
    impl Layout for $float_ty {
        #[inline(always)]
        fn inline_size(_context: &Context) -> usize { mem::size_of::<$float_ty>() }
        #[inline(always)]
        fn inline_align(_context: &Context) -> usize { mem::size_of::<$float_ty>() }
    }

    impl Encodable for $float_ty {
        #[inline]
        fn encode(&mut self, encoder: &mut Encoder<'_, '_>, offset: usize, _recursion_depth: usize) -> Result<()> {
            encoder.debug_check_bounds::<Self>(offset);
            // Safety: The caller ensures `offset` is valid for writing
            // sizeof($float_ty) bytes. Transmuting *u8 to *f32 or *f64 is safe
            // because we use `write_unaligned`.
            unsafe {
                let ptr = encoder.buf.get_unchecked_mut(offset);
                let float_ptr = mem::transmute::<*mut u8, *mut $float_ty>(ptr);
                float_ptr.write_unaligned(*self);
            }
            Ok(())
        }
    }

    impl Decodable for $float_ty {
        #[inline(always)]
        fn new_empty() -> Self { 0 as $float_ty }

        #[inline]
        fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
            decoder.debug_check_bounds::<Self>(offset);
            // Safety: The caller ensures `offset` is valid for reading
            // sizeof($float_ty) bytes. Transmuting *u8 to *f32 or *f64 is safe
            // because we use `read_unaligned`.
            unsafe {
                let ptr = decoder.buf.get_unchecked(offset);
                let float_ptr = mem::transmute::<*const u8, *const $float_ty>(ptr);
                *self = float_ptr.read_unaligned();
            }
            Ok(())
        }
    }

    impl_slice_encoding_by_iter!($float_ty);
)* } }

// Common code used by impl_slice_encoding_by_{iter,copy}.
macro_rules! impl_slice_encoding_base {
    ($prim_ty:ty) => {
        impl Layout for &[$prim_ty] {
            #[inline(always)]
            fn inline_size(_context: &Context) -> usize {
                16
            }
            #[inline(always)]
            fn inline_align(_context: &Context) -> usize {
                8
            }
        }

        impl Layout for Option<&[$prim_ty]> {
            #[inline(always)]
            fn inline_size(_context: &Context) -> usize {
                16
            }
            #[inline(always)]
            fn inline_align(_context: &Context) -> usize {
                8
            }
        }

        impl Encodable for Option<&[$prim_ty]> {
            #[inline]
            fn encode(
                &mut self,
                encoder: &mut Encoder<'_, '_>,
                offset: usize,
                recursion_depth: usize,
            ) -> Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                match self {
                    None => encode_absent_vector(encoder, offset, recursion_depth),
                    Some(slice) => slice.encode(encoder, offset, recursion_depth),
                }
            }
        }
    };
}

// Encodes &[T] as a FIDL vector by encoding items one at a time.
macro_rules! impl_slice_encoding_by_iter {
    ($prim_ty:ty) => {
        impl_slice_encoding_base!($prim_ty);

        impl Encodable for &[$prim_ty] {
            #[inline]
            fn encode(
                &mut self,
                encoder: &mut Encoder<'_, '_>,
                offset: usize,
                recursion_depth: usize,
            ) -> Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                encode_vector_from_iter(
                    encoder,
                    offset,
                    recursion_depth,
                    Some(self.iter().copied()),
                )
            }
        }
    };
}

// Encodes &[T] as a FIDL vector by simple copy.
macro_rules! impl_slice_encoding_by_copy {
    ($prim_ty:ty) => {
        impl_slice_encoding_base!($prim_ty);

        impl Encodable for &[$prim_ty] {
            #[inline]
            fn encode(
                &mut self,
                encoder: &mut Encoder<'_, '_>,
                offset: usize,
                recursion_depth: usize,
            ) -> Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                (self.len() as u64).encode(encoder, offset, recursion_depth)?;
                ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
                Encoder::check_recursion_depth(recursion_depth + 1)?;
                if self.len() == 0 {
                    return Ok(());
                }
                // Encode by simple copy. See Layout::supports_simple_copy for more info.
                let bytes = zerocopy::AsBytes::as_bytes(*self);
                encoder.append_out_of_line_bytes(bytes);
                Ok(())
            }
        }
    };
}

impl_codable_int!(u16, u32, u64, i16, i32, i64,);
impl_codable_float!(f32, f64,);

impl_layout!(bool, align: 1, size: 1);

impl Encodable for bool {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        _recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        // Safety: The caller ensures `offset` is valid for writing 1 byte.
        unsafe {
            // From https://doc.rust-lang.org/std/primitive.bool.html: "If you
            // cast a bool into an integer, true will be 1 and false will be 0."
            *encoder.buf.get_unchecked_mut(offset) = *self as u8;
        }
        Ok(())
    }
}

impl Decodable for bool {
    #[inline(always)]
    fn new_empty() -> Self {
        false
    }
    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        // Safety: The caller ensures `offset` is valid for reading 1 byte.
        *self = match unsafe { *decoder.buf.get_unchecked(offset) } {
            0 => false,
            1 => true,
            _ => return Err(Error::Invalid),
        };
        Ok(())
    }
}

impl_layout_int!(u8);
impl_slice_encoding_by_copy!(u8);

impl Encodable for u8 {
    #[inline(always)]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        _recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        // Safety: The caller ensures `offset` is valid for writing 1 byte.
        unsafe {
            *encoder.buf.get_unchecked_mut(offset) = *self;
        }
        Ok(())
    }
}

impl Decodable for u8 {
    #[inline(always)]
    fn new_empty() -> Self {
        0
    }

    #[inline(always)]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        // Safety: The caller ensures `offset` is valid for reading 1 byte.
        *self = unsafe { *decoder.buf.get_unchecked(offset) };
        Ok(())
    }
}

impl_layout_int!(i8);
impl_slice_encoding_by_copy!(i8);

impl Encodable for i8 {
    #[inline(always)]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        _recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        // Safety: The caller ensures `offset` is valid for writing 1 byte.
        // Transmuting is safe because u8 and i8 have the same size/alignment.
        unsafe {
            *mem::transmute::<*mut u8, *mut i8>(encoder.buf.get_unchecked_mut(offset)) = *self;
        }
        Ok(())
    }
}

impl Decodable for i8 {
    #[inline(always)]
    fn new_empty() -> Self {
        0
    }

    #[inline(always)]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        // Safety: The caller ensures `offset` is valid for reading 1 byte.
        // Transmuting is safe because u8 and i8 have the same size/alignment.
        *self =
            unsafe { *mem::transmute::<*const u8, *const i8>(decoder.buf.get_unchecked(offset)) };
        Ok(())
    }
}

/// Encodes `slice` as a FIDL array.
#[inline]
fn encode_array<T: Encodable>(
    slice: &mut [T],
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
) -> Result<()> {
    let stride = encoder.inline_size_of::<T>();
    let len = slice.len();
    if T::supports_simple_copy() {
        debug_assert_eq!(stride, mem::size_of::<T>());
        // Safety:
        // - The caller ensures `offset` if valid for writing `stride` bytes
        //   (inline size of `T`) `len` times, i.e. `len * stride`.
        // - Since Layout::inline_size is the same as mem::size_of for simple
        //   copy types, `slice` also has exactly `len * stride` bytes.
        // - Rust guarantees `slice` and `encoder.buf` do not alias.
        unsafe {
            let src = slice.as_ptr() as *const u8;
            let dst: *mut u8 = encoder.buf.get_unchecked_mut(offset);
            ptr::copy_nonoverlapping(src, dst, len * stride);
        }
    } else {
        for i in 0..len {
            // Safety: `i` is in bounds since `len` is defined as `slice.len()`.
            let item = unsafe { slice.get_unchecked_mut(i) };
            item.encode(encoder, offset + i * stride, recursion_depth)?;
        }
    }
    Ok(())
}

/// Decodes a FIDL array into `slice`.
#[inline]
fn decode_array<T: Decodable>(
    slice: &mut [T],
    decoder: &mut Decoder<'_>,
    offset: usize,
) -> Result<()> {
    let stride = decoder.inline_size_of::<T>();
    let len = slice.len();
    if T::supports_simple_copy() {
        debug_assert_eq!(stride, mem::size_of::<T>());
        // Safety:
        // - The caller ensures `offset` if valid for reading `stride` bytes
        //   (inline size of `T`) `len` times, i.e. `len * stride`.
        // - Since Layout::inline_size is the same as mem::size_of for simple
        //   copy types, `slice` also has exactly `len * stride` bytes.
        // - Rust guarantees `slice` and `decoder.buf` do not alias.
        unsafe {
            let src: *const u8 = decoder.buf.get_unchecked(offset);
            let dst = slice.as_mut_ptr() as *mut u8;
            ptr::copy_nonoverlapping(src, dst, len * stride);
        }
    } else {
        for i in 0..len {
            // Safety: `i` is in bounds since `len` is defined as `slice.len()`.
            let item = unsafe { slice.get_unchecked_mut(i) };
            item.decode(decoder, offset + i * stride)?;
        }
    }
    Ok(())
}

impl<T: Layout, const N: usize> Layout for [T; N] {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        T::inline_align(context)
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        T::inline_size(context) * N
    }
}

impl<T: Encodable, const N: usize> Encodable for [T; N] {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_array(self, encoder, offset, recursion_depth)
    }
}

impl<T: Decodable, const N: usize> Decodable for [T; N] {
    #[inline]
    fn new_empty() -> Self {
        let mut arr = mem::MaybeUninit::<[T; N]>::uninit();
        unsafe {
            let arr_ptr = arr.as_mut_ptr() as *mut T;
            for i in 0..N {
                ptr::write(arr_ptr.offset(i as isize), T::new_empty());
            }
            arr.assume_init()
        }
    }

    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        decode_array(self, decoder, offset)
    }
}

/// Encode an optional vector-like component.
#[inline]
fn encode_vector<T: Encodable>(
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
    slice_opt: Option<&mut [T]>,
) -> Result<()> {
    match slice_opt {
        Some(slice) => {
            // Two u64: (len, present)
            (slice.len() as u64).encode(encoder, offset, recursion_depth)?;
            ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
            // write_out_of_line must not be called with a zero-sized out-of-line block.
            if slice.len() == 0 {
                return Ok(());
            }
            let bytes_len = slice.len() * encoder.inline_size_of::<T>();
            encoder.write_out_of_line(
                bytes_len,
                recursion_depth,
                |encoder, offset, recursion_depth| {
                    encode_array(slice, encoder, offset, recursion_depth)
                },
            )
        }
        None => encode_absent_vector(encoder, offset, recursion_depth),
    }
}

/// Encode an missing vector-like component.
#[inline]
fn encode_absent_vector(
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
) -> Result<()> {
    0u64.encode(encoder, offset, recursion_depth)?;
    ALLOC_ABSENT_U64.clone().encode(encoder, offset + 8, recursion_depth)
}

/// Like `encode_vector`, but optimized for `&[u8]`.
#[inline]
fn encode_vector_from_bytes(
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
    slice_opt: Option<&[u8]>,
) -> Result<()> {
    match slice_opt {
        Some(slice) => {
            // Two u64: (len, present)
            (slice.len() as u64).encode(encoder, offset, recursion_depth)?;
            ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
            Encoder::check_recursion_depth(recursion_depth + 1)?;
            encoder.append_out_of_line_bytes(slice);
            Ok(())
        }
        None => encode_absent_vector(encoder, offset, recursion_depth),
    }
}

/// Like `encode_vector`, but encodes from an iterator.
#[inline]
fn encode_vector_from_iter<Iter, T>(
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
    iter_opt: Option<Iter>,
) -> Result<()>
where
    Iter: ExactSizeIterator<Item = T>,
    T: Encodable,
{
    match iter_opt {
        Some(iter) => {
            // Two u64: (len, present)
            (iter.len() as u64).encode(encoder, offset, recursion_depth)?;
            ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
            if iter.len() == 0 {
                return Ok(());
            }
            let stride = encoder.inline_size_of::<T>();
            let bytes_len = iter.len() * stride;
            encoder.write_out_of_line(
                bytes_len,
                recursion_depth,
                |encoder, offset, recursion_depth| {
                    for (i, mut item) in iter.enumerate() {
                        item.encode(encoder, offset + stride * i, recursion_depth)?;
                    }
                    Ok(())
                },
            )
        }
        None => encode_absent_vector(encoder, offset, recursion_depth),
    }
}

/// Attempts to decode a string into `string`, returning a `bool`
/// indicating whether or not a string was present.
#[inline]
fn decode_string(decoder: &mut Decoder<'_>, string: &mut String, offset: usize) -> Result<bool> {
    let mut len: u64 = 0;
    len.decode(decoder, offset)?;

    let mut present: u64 = 0;
    present.decode(decoder, offset + 8)?;

    match present {
        ALLOC_PRESENT_U64 => {}
        ALLOC_ABSENT_U64 => {
            return if len == 0 { Ok(false) } else { Err(Error::UnexpectedNullRef) }
        }
        _ => return Err(Error::InvalidPresenceIndicator),
    };
    let len = len as usize;
    decoder.read_out_of_line(len, |decoder, offset| {
        let bytes = &decoder.buf[offset..offset + len];
        let utf8 = str::from_utf8(bytes).map_err(|_| Error::Utf8Error)?;
        let boxed_utf8: Box<str> = utf8.into();
        *string = boxed_utf8.into_string();
        Ok(true)
    })
}

/// Attempts to decode a FIDL vector into `vec`, returning a `bool` indicating
/// whether the vector was present.
#[inline]
fn decode_vector<T: Decodable>(
    decoder: &mut Decoder<'_>,
    vec: &mut Vec<T>,
    offset: usize,
) -> Result<bool> {
    let mut len: u64 = 0;
    len.decode(decoder, offset)?;

    let mut present: u64 = 0;
    present.decode(decoder, offset + 8)?;

    match present {
        ALLOC_PRESENT_U64 => {}
        ALLOC_ABSENT_U64 => {
            return if len == 0 { Ok(false) } else { Err(Error::UnexpectedNullRef) }
        }
        _ => return Err(Error::InvalidPresenceIndicator),
    }

    let len = len as usize;
    let bytes_len = len * decoder.inline_size_of::<T>();
    decoder.read_out_of_line(bytes_len, |decoder, offset| {
        if T::supports_simple_copy() {
            // Safety: The uninitalized elements are immediately written by
            // `decode_array`, which always succeeds in the simple copy case.
            unsafe {
                resize_vec_no_zeroing(vec, len);
            }
        } else {
            vec.resize_with(len, T::new_empty);
        }
        // Safety: `vec` has `len` elements based on the above code.
        decode_array(vec, decoder, offset)?;
        Ok(true)
    })
}

impl_layout!(&str, align: 8, size: 16);

impl Encodable for &str {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector_from_bytes(encoder, offset, recursion_depth, Some(self.as_bytes()))
    }
}

impl_layout!(String, align: 8, size: 16);

impl Encodable for String {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector_from_bytes(encoder, offset, recursion_depth, Some(self.as_bytes()))
    }
}

impl Decodable for String {
    #[inline(always)]
    fn new_empty() -> Self {
        String::new()
    }

    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        if decode_string(decoder, self, offset)? {
            Ok(())
        } else {
            Err(Error::NotNullable)
        }
    }
}

impl_layout!(Option<&str>, align: 8, size: 16);

impl Encodable for Option<&str> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector_from_bytes(
            encoder,
            offset,
            recursion_depth,
            self.as_ref().map(|x| x.as_bytes()),
        )
    }
}

impl_layout!(Option<String>, align: 8, size: 16);

impl Encodable for Option<String> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector_from_bytes(
            encoder,
            offset,
            recursion_depth,
            self.as_ref().map(|x| x.as_bytes()),
        )
    }
}

impl Decodable for Option<String> {
    #[inline(always)]
    fn new_empty() -> Self {
        None
    }

    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        let was_some;
        {
            let string = self.get_or_insert(String::new());
            was_some = decode_string(decoder, string, offset)?;
        }
        if !was_some {
            *self = None
        }
        Ok(())
    }
}

impl_layout_forall_T!(&mut dyn ExactSizeIterator<Item = T>, align: 8, size: 16);

impl<T: Encodable> Encodable for &mut dyn ExactSizeIterator<Item = T> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector_from_iter(encoder, offset, recursion_depth, Some(self))
    }
}

impl_layout_forall_T!(Vec<T>, align: 8, size: 16);

impl<T: Encodable> Encodable for Vec<T> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector(encoder, offset, recursion_depth, Some(self))
    }
}

impl<T: Decodable> Decodable for Vec<T> {
    #[inline(always)]
    fn new_empty() -> Self {
        Vec::new()
    }

    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        if decode_vector(decoder, self, offset)? {
            Ok(())
        } else {
            Err(Error::NotNullable)
        }
    }
}

impl_layout_forall_T!(Option<&mut dyn ExactSizeIterator<Item = T>>, align: 8, size: 16);

impl<T: Encodable> Encodable for Option<&mut dyn ExactSizeIterator<Item = T>> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector_from_iter(encoder, offset, recursion_depth, self.as_mut().map(|x| &mut **x))
    }
}

impl_layout_forall_T!(Option<Vec<T>>, align: 8, size: 16);

impl<T: Encodable> Encodable for Option<Vec<T>> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        encode_vector(encoder, offset, recursion_depth, self.as_deref_mut())
    }
}

impl<T: Decodable> Decodable for Option<Vec<T>> {
    #[inline(always)]
    fn new_empty() -> Self {
        None
    }

    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        let was_some;
        {
            let vec = self.get_or_insert(Vec::new());
            was_some = decode_vector(decoder, vec, offset)?;
        }
        if !was_some {
            *self = None
        }
        Ok(())
    }
}

/// Implements the FIDL `Encodable` and `Decodable` traits for a struct
/// representing a FIDL bits type. The struct must have been generated by the
/// bitflags crate.
#[macro_export]
macro_rules! fidl_bits {
    (
        name: $name:ident,
        prim_ty: $prim_ty:ty,
        // Provide `strict: true` or `flexible: true`, not both.
        $(strict: $strict:expr,)?
        $(flexible: $flexible:expr,)?
    ) => {
        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(context: &$crate::encoding::Context) -> usize {
                <$prim_ty as $crate::encoding::Layout>::inline_align(context)
            }

            #[inline(always)]
            fn inline_size(context: &$crate::encoding::Context) -> usize {
                <$prim_ty as $crate::encoding::Layout>::inline_size(context)
            }
        }

        impl $crate::encoding::Encodable for $name {
            #[inline]
            fn encode(
                &mut self,
                encoder: &mut $crate::encoding::Encoder<'_, '_>,
                offset: usize,
                recursion_depth: usize,
            ) -> std::result::Result<(), $crate::Error> {
                encoder.debug_check_bounds::<Self>(offset);
                $(
                    $strict; // placeholder use for expansion
                    if self.bits & Self::all().bits != self.bits {
                        return Err($crate::Error::InvalidBitsValue);
                    }
                )?
                self.bits.encode(encoder, offset, recursion_depth)
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline(always)]
            fn new_empty() -> Self {
                Self::empty()
            }

            #[inline]
            fn decode(
                &mut self,
                decoder: &mut $crate::encoding::Decoder<'_>,
                offset: usize,
            ) -> std::result::Result<(), $crate::Error> {
                decoder.debug_check_bounds::<Self>(offset);
                let mut prim = <$prim_ty>::new_empty();
                prim.decode(decoder, offset)?;
                $(
                    $strict; // placeholder use for expansion
                    *self = Self::from_bits(prim).ok_or($crate::Error::InvalidBitsValue)?;
                )?
                $(
                    $flexible; // placeholder use for expansion
                    *self = Self::from_bits_allow_unknown(prim);
                )?
                Ok(())
            }
        }
    };
}

/// Implements the FIDL `Encodable` and `Decodable` traits for an enum
/// representing a FIDL strict enum.
#[macro_export]
macro_rules! fidl_enum {
    (
        name: $name:ident,
        prim_ty: $prim_ty:ty,
        // Provide `strict: true` or `flexible: true`, not both.
        $(
            strict: $strict:expr,
            min_member: $min_member:ident,
        )?
        $(flexible: $flexible:expr,)?
    ) => {
        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(context: &$crate::encoding::Context) -> usize {
                <$prim_ty as $crate::encoding::Layout>::inline_align(context)
            }

            #[inline(always)]
            fn inline_size(context: &$crate::encoding::Context) -> usize {
                <$prim_ty as $crate::encoding::Layout>::inline_size(context)
            }
        }

        impl $crate::encoding::Encodable for $name {
            #[inline]
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, recursion_depth: usize)
                -> std::result::Result<(), $crate::Error>
            {
                encoder.debug_check_bounds::<Self>(offset);
                self.into_primitive().encode(encoder, offset, recursion_depth)
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline(always)]
            fn new_empty() -> Self {
                $(
                    $strict; // placeholder use for expansion
                    return Self::$min_member;
                )?
                $(
                    $flexible; // placeholder use for expansion
                    Self::unknown()
                )?
            }

            #[inline]
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize)
                -> std::result::Result<(), $crate::Error>
            {
                decoder.debug_check_bounds::<Self>(offset);
                let mut prim = <$prim_ty>::new_empty();
                prim.decode(decoder, offset)?;
                $(
                    $strict; // placeholder use for expansion
                    *self = Self::from_primitive(prim).ok_or($crate::Error::InvalidEnumValue)?;
                )?
                $(
                    $flexible; // placeholder use for expansion
                    *self = Self::from_primitive_allow_unknown(prim);
                )?
                Ok(())
            }
        }
    }
}

impl_layout!(Handle, align: 4, size: 4);

impl Encodable for Handle {
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        ALLOC_PRESENT_U32.clone().encode(encoder, offset, recursion_depth)?;
        // fidlc forbids handle types with empty rights.
        debug_assert_ne!(encoder.next_handle_rights, Rights::empty());
        Ok(encoder.handles.push(HandleDisposition {
            handle_op: HandleOp::Move(take_handle(self)),
            object_type: encoder.next_handle_subtype,
            rights: encoder.next_handle_rights,
            result: Status::OK,
        }))
    }
}

impl Decodable for Handle {
    #[inline(always)]
    fn new_empty() -> Self {
        Handle::invalid()
    }

    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        let mut present: u32 = 0;
        present.decode(decoder, offset)?;
        match present {
            ALLOC_PRESENT_U32 => {}
            ALLOC_ABSENT_U32 => return Err(Error::NotNullable),
            _ => return Err(Error::InvalidPresenceIndicator),
        }
        *self = decoder.take_next_handle()?;
        Ok(())
    }
}

impl_layout!(Option<Handle>, align: 4, size: 4);

impl Encodable for Option<Handle> {
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        match self {
            Some(handle) => handle.encode(encoder, offset, recursion_depth),
            None => ALLOC_ABSENT_U32.clone().encode(encoder, offset, recursion_depth),
        }
    }
}

impl Decodable for Option<Handle> {
    #[inline(always)]
    fn new_empty() -> Self {
        None
    }

    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        let mut present: u32 = 0;
        present.decode(decoder, offset)?;
        match present {
            ALLOC_ABSENT_U32 => {
                *self = None;
                Ok(())
            }
            ALLOC_PRESENT_U32 => {
                *self = Some(decoder.take_next_handle()?);
                Ok(())
            }
            _ => Err(Error::InvalidPresenceIndicator),
        }
    }
}

/// A macro for implementing the `Encodable` and `Decodable` traits for a type
/// which implements the `fuchsia_zircon::HandleBased` trait.
// TODO(cramertj) replace when specialization is stable
#[macro_export]
macro_rules! handle_based_codable {
    ($($ty:ident$(:- <$($generic:ident,)*>)*, )*) => { $(
        impl<$($($generic,)*)*> $crate::encoding::Layout for $ty<$($($generic,)*)*> {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize { 4 }
            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize { 4 }
        }

        impl<$($($generic,)*)*> $crate::encoding::Encodable for $ty<$($($generic,)*)*> {

#[inline]
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, recursion_depth: usize)
                -> $crate::Result<()>
            {
                encoder.debug_check_bounds::<Self>(offset);
                let mut handle = $crate::encoding::take_handle(self);
                handle.encode(encoder, offset, recursion_depth)
            }
        }

        impl<$($($generic,)*)*> $crate::encoding::Decodable for $ty<$($($generic,)*)*> {
            #[inline(always)]
            fn new_empty() -> Self {
                <$ty<$($($generic,)*)*> as $crate::handle::HandleBased>::from_handle($crate::handle::Handle::invalid())
            }
            #[inline]
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize)
                -> $crate::Result<()>
            {
                let mut handle = $crate::handle::Handle::invalid();
                handle.decode(decoder, offset)?;
                *self = <$ty<$($($generic,)*)*> as $crate::handle::HandleBased>::from_handle(handle);
                Ok(())
            }
        }

        impl<$($($generic,)*)*> $crate::encoding::Layout for Option<$ty<$($($generic,)*)*>> {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize { 4 }
            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize { 4 }
        }

        impl<$($($generic,)*)*> $crate::encoding::Encodable for Option<$ty<$($($generic,)*)*>> {
            #[inline]
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, recursion_depth: usize)
                -> $crate::Result<()>
            {
                encoder.debug_check_bounds::<Self>(offset);
                match self {
                    Some(handle) => handle.encode(encoder, offset, recursion_depth),
                    None => $crate::encoding::ALLOC_ABSENT_U32.clone().encode(encoder, offset, recursion_depth),
                }
            }
        }

        impl<$($($generic,)*)*> $crate::encoding::Decodable for Option<$ty<$($($generic,)*)*>> {
            #[inline(always)]
            fn new_empty() -> Self { None }
            #[inline]
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                let mut handle: Option<$crate::handle::Handle> = None;
                handle.decode(decoder, offset)?;
                *self = handle.map(Into::into);
                Ok(())
            }
        }
    )* }
}

impl Layout for zx_status::Status {
    #[inline(always)]
    fn inline_size(_context: &Context) -> usize {
        mem::size_of::<zx_status::zx_status_t>()
    }
    #[inline(always)]
    fn inline_align(_context: &Context) -> usize {
        mem::size_of::<zx_status::zx_status_t>()
    }
}

impl Encodable for zx_status::Status {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        self.into_raw().encode(encoder, offset, recursion_depth)
    }
}

impl Decodable for zx_status::Status {
    #[inline(always)]
    fn new_empty() -> Self {
        Self::from_raw(0)
    }
    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        let mut val: i32 = 0;
        val.decode(decoder, offset)?;
        *self = Self::from_raw(val);
        Ok(())
    }
}

/// The body of a FIDL Epitaph
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EpitaphBody {
    /// The error status
    pub error: zx_status::Status,
}

impl Layout for EpitaphBody {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        <zx_status::Status as Layout>::inline_align(context)
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        <zx_status::Status as Layout>::inline_size(context)
    }
}

impl Encodable for EpitaphBody {
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        self.error.encode(encoder, offset, recursion_depth)
    }
}

impl Decodable for EpitaphBody {
    #[inline(always)]
    fn new_empty() -> Self {
        Self { error: zx_status::Status::new_empty() }
    }
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        self.error.decode(decoder, offset)
    }
}

macro_rules! handle_encoding {
    ($x:tt, $docname:expr, $name:ident, $value:expr, $availability:ident) => {
        type $x = crate::handle::$x;
        handle_based_codable![$x,];
    };
}
invoke_for_handle_types!(handle_encoding);

/// A trait that provides automatic support for nullable types.
///
/// Types that implement this trait will automatically receive `Encodable` and
/// `Decodable` implementations for `Option<Box<Self>>` (nullable owned type),
/// and `Encodable` for `Option<&mut Self>` (nullable borrowed type).
pub trait Autonull: Encodable + Decodable {
    /// Returns true if the type is naturally able to be nullable.
    ///
    /// Types that return true (e.g., xunions) encode `Some(x)` the same as `x`,
    /// and `None` as a full bout of inline zeros. Types that return false
    /// (e.g., structs) encode `Some(x)` as `ALLOC_PRESENT_U64` with an
    /// out-of-line payload, and `None` as `ALLOC_ABSENT_U64`.
    fn naturally_nullable(context: &Context) -> bool;
}

impl<T: Autonull> Layout for Option<&mut T> {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        if T::naturally_nullable(context) {
            <T as Layout>::inline_align(context)
        } else {
            8
        }
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        if T::naturally_nullable(context) {
            <T as Layout>::inline_size(context)
        } else {
            8
        }
    }
}

impl<T: Autonull> Encodable for Option<&mut T> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        if T::naturally_nullable(encoder.context) {
            match self {
                Some(x) => x.encode(encoder, offset, recursion_depth),
                None => {
                    // This is an empty xunion.
                    encoder.padding(offset, 24);
                    Ok(())
                }
            }
        } else {
            match self {
                Some(x) => {
                    ALLOC_PRESENT_U64.clone().encode(encoder, offset, recursion_depth)?;
                    encoder.write_out_of_line(
                        encoder.inline_size_of::<T>(),
                        recursion_depth,
                        |encoder, offset, recursion_depth| {
                            x.encode(encoder, offset, recursion_depth)
                        },
                    )
                }
                None => ALLOC_ABSENT_U64.clone().encode(encoder, offset, recursion_depth),
            }
        }
    }
}

impl<T: Autonull> Layout for Option<Box<T>> {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        <Option<&mut T> as Layout>::inline_align(context)
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        <Option<&mut T> as Layout>::inline_size(context)
    }
}

impl<T: Autonull> Encodable for Option<Box<T>> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        // Call Option<&mut T>'s encode method.
        self.as_deref_mut().encode(encoder, offset, recursion_depth)
    }
}

// Presence indicators always include at least one non-zero byte,
// while absence indicators should always be entirely zeros.
#[inline]
fn check_for_presence(
    decoder: &mut Decoder<'_>,
    offset: usize,
    inline_size: usize,
) -> Result<bool> {
    Ok(decoder.buf[offset..offset + inline_size].iter().any(|byte| *byte != 0))
}

impl<T: Autonull> Decodable for Option<Box<T>> {
    #[inline(always)]
    fn new_empty() -> Self {
        None
    }
    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        if T::naturally_nullable(decoder.context) {
            let inline_size = decoder.inline_size_of::<T>();
            let present = check_for_presence(decoder, offset, inline_size)?;
            if present {
                self.get_or_insert_with(|| Box::new(T::new_empty())).decode(decoder, offset)
            } else {
                *self = None;
                // Eat the full `inline_size` bytes including the
                // ALLOC_ABSENT that we only peeked at before
                decoder.check_padding(offset, inline_size)?;
                Ok(())
            }
        } else {
            let mut present: u64 = 0;
            present.decode(decoder, offset)?;
            match present {
                ALLOC_PRESENT_U64 => {
                    decoder.read_out_of_line(decoder.inline_size_of::<T>(), |decoder, offset| {
                        self.get_or_insert_with(|| Box::new(T::new_empty())).decode(decoder, offset)
                    })
                }
                ALLOC_ABSENT_U64 => {
                    *self = None;
                    Ok(())
                }
                _ => Err(Error::InvalidPresenceIndicator),
            }
        }
    }
}

/// Implements the FIDL `Encodable` and `Decodable` traits for a struct
/// representing a FIDL struct.
#[macro_export]
macro_rules! fidl_struct {
    (
        name: $name:ty,
        members: [$(
            $member_name:ident {
                ty: $member_ty:ty,
                offset_v1: $member_offset_v1:expr,
                $(
                    handle_metadata: {
                        handle_subtype: $member_handle_subtype:expr,
                        handle_rights: $member_handle_rights:expr,
                    },
                )?
            },
        )*],
        padding: [$(
            {
                ty: $padding_ty:ty,
                offset: $padding_offset:expr,
                mask: $padding_mask:expr,
            },
        )*],
        size_v1: $size_v1:expr,
        align_v1: $align_v1:expr,
    ) => {
        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize {
                $align_v1
            }

            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize {
                $size_v1
            }
        }

        impl $crate::encoding::Encodable for $name {
            #[inline]
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, recursion_depth: usize) -> $crate::Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                // Ensure padding is zero by writing zero to the padded region which will be partially overwritten
                // when the field is written.
                $(
                    unsafe {
                        let ptr = encoder.mut_buffer().as_mut_ptr().offset(offset as isize).offset($padding_offset);
                        std::mem::transmute::<*mut u8, *mut $padding_ty>(ptr).write_unaligned(0);
                    }
                )*
                // Write the fields.
                $(
                    $(
                        encoder.set_next_handle_subtype($member_handle_subtype);
                        encoder.set_next_handle_rights($member_handle_rights);
                    )?
                    self.$member_name.encode(encoder, offset + $member_offset_v1, recursion_depth)?;
                )*
                Ok(())
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline]
            fn new_empty() -> Self {
                Self {
                    $(
                        $member_name: <$member_ty>::new_empty(),
                    )*
                }
            }

            #[inline]
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                // Apply masks to check if padded regions are zero.
                $(
                    let ptr = unsafe { decoder.buffer().as_ptr().offset(offset as isize).offset($padding_offset) };
                    let padval = unsafe { std::mem::transmute::<*const u8, *const $padding_ty>(ptr).read_unaligned() };
                    let maskedval = padval & $padding_mask;
                    if (maskedval != 0) {
                        return Err($crate::Error::NonZeroPadding {
                            padding_start: offset + $padding_offset + (($padding_mask as u64).trailing_zeros() / 8) as usize,
                            non_zero_pos: offset + $padding_offset + (maskedval.trailing_zeros() / 8) as usize
                        });
                    }
                )*
                // Read the fields.
                $(
                    $(
                        decoder.set_next_handle_subtype($member_handle_subtype);
                        decoder.set_next_handle_rights($member_handle_rights);
                    )?
                    self.$member_name.decode(decoder, offset + $member_offset_v1)?;
                )*
                Ok(())
            }
        }

        impl $crate::encoding::Autonull for $name {
            #[inline(always)]
            fn naturally_nullable(_context: &$crate::encoding::Context) -> bool {
                false
            }
        }
    }
}

/// Implements the FIDL `Encodable` and `Decodable` traits for a struct
/// representing a FIDL struct, encoding and decoding by simple copy. See
/// Layout::supports_simple_copy for more info.
#[macro_export]
macro_rules! fidl_struct_copy {
    (
        name: $name:ty,
        members: [$(
            $member_name:ident {
                ty: $member_ty:ty,
                offset_v1: $member_offset_v1:expr,
            },
        )*],
        padding: [$(
            {
                ty: $padding_ty:ty,
                offset: $padding_offset:expr,
                mask: $padding_mask:expr,
            },
        )*],
        size_v1: $size_v1:expr,
        align_v1: $align_v1:expr,
    ) => {
        static_assertions::const_assert_eq!(std::mem::size_of::<$name>(), $size_v1);
        static_assertions::const_assert_eq!(std::mem::align_of::<$name>(), $align_v1);

        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize {
                $align_v1
            }

            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize {
                $size_v1
            }

            #[inline(always)]
            fn supports_simple_copy() -> bool {
                #![allow(unreachable_code)]
                // Copy as byte array if there is no padding.
                $(
                    $padding_offset; // Force this to be the padding repeat list.
                    return false;
                )*
                true
            }
        }

        impl $crate::encoding::Encodable for $name {
            #[inline]
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, _recursion_depth: usize) -> $crate::Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                unsafe {
                    let buf_ptr = encoder.mut_buffer().as_mut_ptr().offset(offset as isize);
                    let typed_buf_ptr = std::mem::transmute::<*mut u8, *mut $name>(buf_ptr);
                    typed_buf_ptr.write_unaligned((self as *const $name).read());
                    $(
                        let ptr = buf_ptr.offset($padding_offset);
                        let padding_ptr = std::mem::transmute::<*mut u8, *mut $padding_ty>(ptr);
                        padding_ptr.write_unaligned(padding_ptr.read_unaligned() & !$padding_mask);
                    )*
                }
                Ok(())
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline]
            fn new_empty() -> Self {
                Self {
                    $(
                        $member_name: <$member_ty>::new_empty(),
                    )*
                }
            }

            #[inline]
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                let buf_ptr = unsafe { decoder.buffer().as_ptr().offset(offset as isize) };

                // Apply masks to check if padded regions are zero.
                $(
                    let ptr = unsafe { buf_ptr.offset($padding_offset) };
                    let padval = unsafe { std::mem::transmute::<*const u8, *const $padding_ty>(ptr).read_unaligned() };
                    let maskedval = padval & $padding_mask;
                    if (maskedval != 0) {
                        return Err($crate::Error::NonZeroPadding {
                            padding_start: offset + $padding_offset + (($padding_mask as u64).trailing_zeros() / 8) as usize,
                            non_zero_pos: offset + $padding_offset + (maskedval.trailing_zeros() / 8) as usize
                        });
                    }
                )*

                unsafe {
                    let obj_ptr = std::mem::transmute::<*mut $name, *mut u8>(self);
                    std::ptr::copy_nonoverlapping(buf_ptr, obj_ptr, $size_v1);
                }

                Ok(())
            }
        }

        impl $crate::encoding::Autonull for $name {
            #[inline(always)]
            fn naturally_nullable(_context: &$crate::encoding::Context) -> bool {
                false
            }
        }
    }
}

/// Implements the FIDL `Encodable` and `Decodable` traits for a struct having
/// the form `pub struct Name;` representing an empty FIDL struct.
#[macro_export]
macro_rules! fidl_empty_struct {
    ($name:ident) => {
        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize {
                1
            }
            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize {
                1
            }
        }

        impl $crate::encoding::Encodable for $name {
            #[inline]
            fn encode(
                &mut self,
                encoder: &mut $crate::encoding::Encoder<'_, '_>,
                offset: usize,
                recursion_depth: usize,
            ) -> $crate::Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                0u8.encode(encoder, offset, recursion_depth)
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline(always)]
            fn new_empty() -> Self {
                $name
            }
            #[inline]
            fn decode(
                &mut self,
                decoder: &mut $crate::encoding::Decoder<'_>,
                offset: usize,
            ) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                let mut x = 0u8;
                x.decode(decoder, offset)?;
                if x == 0 {
                    Ok(())
                } else {
                    Err($crate::Error::Invalid)
                }
            }
        }

        impl $crate::encoding::Autonull for $name {
            #[inline(always)]
            fn naturally_nullable(_context: &$crate::encoding::Context) -> bool {
                false
            }
        }
    };
}

/// Encode the provided unknown bytes and handles behind a FIDL "envelope".
#[doc(hidden)] // only exported for macro use
pub fn encode_unknown_data(
    val: &mut UnknownData,
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
) -> Result<()> {
    (val.bytes.len() as u32).encode(encoder, offset, recursion_depth)?;
    (val.handles.len() as u32).encode(encoder, offset + 4, recursion_depth)?;
    ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
    Encoder::check_recursion_depth(recursion_depth + 1)?;
    encoder.append_out_of_line_bytes(&val.bytes);
    encoder.set_next_handle_subtype(ObjectType::NONE);
    encoder.set_next_handle_rights(Rights::SAME_RIGHTS);
    encoder.append_unknown_handles(&mut val.handles);
    Ok(())
}

/// Encode the provided unknown bytes behind a FIDL "envelope".
#[doc(hidden)] // only exported for macro use
pub fn encode_unknown_bytes(
    val: &[u8],
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
) -> Result<()> {
    (val.len() as u64).encode(encoder, offset, recursion_depth)?;
    ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
    Encoder::check_recursion_depth(recursion_depth + 1)?;
    encoder.append_out_of_line_bytes(val);
    Ok(())
}

/// Encode the provided value behind a FIDL "envelope".
#[doc(hidden)] // only exported for macro use
pub fn encode_in_envelope(
    val: &mut Option<&mut dyn Encodable>,
    encoder: &mut Encoder<'_, '_>,
    offset: usize,
    recursion_depth: usize,
) -> Result<()> {
    // u32 num_bytes
    // u32 num_handles
    // 64-bit presence indicator

    match val {
        Some(x) => {
            // Start at offset 8 because we write the first 8 bytes (number of bytes and number
            // number of handles, both u32) at the end.
            ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
            let bytes_before = encoder.buf.len();
            let handles_before = encoder.handles.len();
            encoder.write_out_of_line(
                x.inline_size(encoder.context),
                recursion_depth,
                |e, offset, recursion_depth| x.encode(e, offset, recursion_depth),
            )?;
            let mut bytes_written = (encoder.buf.len() - bytes_before) as u32;
            let mut handles_written = (encoder.handles.len() - handles_before) as u32;
            bytes_written.encode(encoder, offset, recursion_depth)?;
            handles_written.encode(encoder, offset + 4, recursion_depth)?;
        }
        None => {
            0u32.encode(encoder, offset, recursion_depth)?; // num_bytes
            0u32.encode(encoder, offset + 4, recursion_depth)?; // num_handles
            ALLOC_ABSENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
        }
    }
    Ok(())
}

/// Decodes the unknown bytes behind a FIDL "envelope", erroring if there are
/// any handles. The result is None if the envelope is absent.
#[doc(hidden)] // only exported for macro use
#[inline]
pub fn decode_unknown_bytes(decoder: &mut Decoder<'_>, offset: usize) -> Result<Option<Vec<u8>>> {
    let mut num_bytes: u32 = 0;
    num_bytes.decode(decoder, offset)?;
    let mut num_handles: u32 = 0;
    num_handles.decode(decoder, offset + 4)?;
    let mut present: u64 = 0;
    present.decode(decoder, offset + 8)?;

    match present {
        ALLOC_PRESENT_U64 => {
            if num_handles != 0 {
                for _ in 0..num_handles {
                    decoder.drop_next_handle()?;
                }
                return Err(Error::CannotStoreUnknownHandles);
            }
            decoder.read_out_of_line(num_bytes as usize, |decoder, offset| {
                Ok(Some(decoder.buffer()[offset..offset + (num_bytes as usize)].to_vec()))
            })
        }
        ALLOC_ABSENT_U64 => {
            if num_bytes != 0 {
                Err(Error::InvalidNumBytesInEnvelope)
            } else if num_handles != 0 {
                Err(Error::InvalidNumHandlesInEnvelope)
            } else {
                Ok(None)
            }
        }
        _ => Err(Error::InvalidPresenceIndicator),
    }
}

/// Decodes the unknown bytes and handles behind a FIDL "envelope". The result
/// is None if the envelope is absent.
#[doc(hidden)] // only exported for macro use
#[inline]
pub fn decode_unknown_data(
    decoder: &mut Decoder<'_>,
    offset: usize,
) -> Result<Option<UnknownData>> {
    let mut num_bytes: u32 = 0;
    num_bytes.decode(decoder, offset)?;
    let mut num_handles: u32 = 0;
    num_handles.decode(decoder, offset + 4)?;
    let mut present: u64 = 0;
    present.decode(decoder, offset + 8)?;

    match present {
        ALLOC_PRESENT_U64 => decoder.read_out_of_line(num_bytes as usize, |decoder, offset| {
            decode_unknown_data_contents(decoder, offset, num_bytes, num_handles).map(Some)
        }),
        ALLOC_ABSENT_U64 => {
            if num_bytes != 0 {
                Err(Error::InvalidNumBytesInEnvelope)
            } else if num_handles != 0 {
                Err(Error::InvalidNumHandlesInEnvelope)
            } else {
                Ok(None)
            }
        }
        _ => Err(Error::InvalidPresenceIndicator),
    }
}

/// Returns the unknown bytes and handles directly from an envelope's out of line data.
#[doc(hidden)] // only exported for macro use
#[inline]
pub fn decode_unknown_data_contents(
    decoder: &mut Decoder<'_>,
    offset: usize,
    num_bytes: u32,
    num_handles: u32,
) -> Result<UnknownData> {
    let bytes = decoder.buffer()[offset..offset + (num_bytes as usize)].to_vec();
    let mut handles = Vec::with_capacity(num_handles as usize);
    decoder.set_next_handle_subtype(ObjectType::NONE);
    decoder.set_next_handle_rights(Rights::SAME_RIGHTS);
    for _ in 0..num_handles {
        handles.push(decoder.take_next_handle()?);
    }
    Ok(UnknownData { bytes, handles })
}

/// Implements the FIDL `Encodable` and `Decodable` traits for a struct
/// representing a FIDL table. All the struct's fields must be `Option`s, except
/// for the `pub __non_exhaustive: ()` field. Also generates the `empty()` method
/// returning an empty table.
#[macro_export]
macro_rules! fidl_table {
    (
        name: $name:ty,
        members: [$(
            // NOTE: members are in order from lowest to highest ordinal
            $member_name:ident {
                ty: $member_ty:ty,
                ordinal: $ordinal:expr,
                $(
                    handle_metadata: {
                        handle_subtype: $member_handle_subtype:expr,
                        handle_rights: $member_handle_rights:expr,
                    },
                )?
            },
        )*],
        // The resource_unknown_member and value_unknown_member represent the
        // name of the unknown variant for resource types and value types
        // respectively. Exactly one of the two fields must be set.
        $( resource_unknown_member: $resource_unknown_name:ident, )?
        $( value_unknown_member: $value_unknown_name:ident, )?
    ) => {
        impl $name {
            // Note: this value is implemented as a constant since this
            // is currently the only way that the Rust compiler can directly
            // construct members when using the FRU syntax. For example in:
            //
            // SomeTable { some_member: some_value, ..SomeTable::empty() }
            //
            // Using a constant (::EMPTY) will only create members that do not
            // have a value explicitly specified, whereas using a function
            // (::EMPTY()) will fully instantiate an instance of the struct and
            // overwrite the members that do not have a value specified. The
            // latter makes it impossible to construct a const value because
            // overwiting heap allocated values (even if they are optional and
            // are None) will call their destructor which is not const.
            //
            // Workarounds, like implementing ::EMPTY in terms of empty() or
            // the other way around do not work either
            // https://play.rust-lang.org/?version=stable&mode=debug&edition=2018&gist=697cddc6b8c34464ffaeb15584f2618e
            /// An empty table with every field set to `None`.
            #[allow(deprecated)]
            pub const EMPTY: Self = Self {
                $(
                    $member_name: None,
                )*
                $(
                    $resource_unknown_name: None,
                )?
                $(
                    $value_unknown_name: None,
                )?
                __non_exhaustive: (),
            };

            #[inline(always)]
            fn find_max_ordinal(&self) -> u64 {
                std::cmp::max(self.find_max_known_ordinal(), self.find_max_unknown_ordinal())
            }

            #[inline(always)]
            fn find_max_known_ordinal(&self) -> u64 {
                $crate::fidl_reverse_blocks!{$({
                    if let Some(_) = self.$member_name {
                        return $ordinal;
                    }
                })*}
                0
            }

            #[inline(always)]
            fn find_max_unknown_ordinal(&self) -> u64 {
                // unknown data must either be None or Some of a non-empty map (i.e. it cannot
                // have an additional empty state of Some of an empty map), so we can unwrap the
                // result of .max()
                $(
                    // TODO: When https://github.com/rust-lang/rust/issues/62924 is fixed,
                    // change this to data.keys().last_key_value().unwrap().0.
                    self.$value_unknown_name.as_ref().map_or(0, |data| *data.keys().next_back().unwrap())
                )?
                $(
                    // TODO: When https://github.com/rust-lang/rust/issues/62924 is fixed,
                    // change this to data.keys().last_key_value().unwrap().0.
                    self.$resource_unknown_name.as_ref().map_or(0, |data| *data.keys().next_back().unwrap())
                )?
            }
        }

        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize { 8 }
            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize { 16 }
        }

        impl $crate::encoding::Encodable for $name {
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, recursion_depth: usize) -> $crate::Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                // Vector header
                let max_ordinal = self.find_max_ordinal();
                (max_ordinal as u64).encode(encoder, offset, recursion_depth)?;
                $crate::encoding::ALLOC_PRESENT_U64.clone().encode(encoder, offset + 8, recursion_depth)?;
                // write_out_of_line must not be called with a zero-sized out-of-line block.
                if max_ordinal == 0 {
                    return Ok(());
                }
                let bytes_len = (max_ordinal as usize) * 16;
                encoder.write_out_of_line(bytes_len, recursion_depth, |_encoder, _offset, _recursion_depth| {
                    $(
                        let mut _unknown_fields = self.$value_unknown_name.iter().flatten();
                        let mut _next_unknown = _unknown_fields.next();
                        let _unknown_encoder_func = $crate::encoding::encode_unknown_bytes;
                    )?
                    $(
                        let mut _unknown_fields = self.$resource_unknown_name.iter_mut().flatten();
                        let mut _next_unknown = _unknown_fields.next();
                        let _unknown_encoder_func = $crate::encoding::encode_unknown_data;
                    )?
                    let mut _prev_end_offset: usize = 0;
                    $(
                        // Encode unknown envelopes for gaps in ordinals
                        while let Some((ordinal, data)) = _next_unknown.as_mut() {
                            if **ordinal > $ordinal {
                                break;
                            }
                            let cur_offset: usize = (**ordinal as usize - 1) * 16;
                            // Zero reserved fields.
                            _encoder.padding(_offset + _prev_end_offset, cur_offset - _prev_end_offset);

                            // Safety:
                            // - bytes_len is calculated to fit 16*max(member.ordinal).
                            // - Since cur_offset is 16*(member.ordinal - 1) and the envelope takes
                            //   16 bytes, there is always sufficient room.
                            _unknown_encoder_func(*data, _encoder, _offset + cur_offset, _recursion_depth)?;
                            _next_unknown = _unknown_fields.next();
                            _prev_end_offset = cur_offset + 16;
                        }
                        if $ordinal > max_ordinal {
                            return Ok(());
                        }

                        // Write at offset+(ordinal-1)*16, since ordinals are one-based and envelopes are 16 bytes.
                        let cur_offset: usize = ($ordinal - 1) * 16;

                        // Zero reserved fields.
                        _encoder.padding(_offset + _prev_end_offset, cur_offset - _prev_end_offset);

                        // Encode field.
                        $(
                            _encoder.set_next_handle_subtype($member_handle_subtype);
                            _encoder.set_next_handle_rights($member_handle_rights);
                        )?
                        // Safety:
                        // - bytes_len is calculated to fit 16*max(member.ordinal).
                        // - Since cur_offset is 16*(member.ordinal - 1) and the envelope takes
                        //   16 bytes, there is always sufficient room.
                        let mut field = self.$member_name.as_mut().map(|x| x as &mut dyn $crate::encoding::Encodable);
                        $crate::encoding::encode_in_envelope(&mut field, _encoder, _offset + cur_offset, _recursion_depth)?;

                        _prev_end_offset = cur_offset + 16;
                    )*

                    // Encode the remaining unknown envelopes. We use a while loop here instead of
                    // a for loop on `_unknown_fields`, because there might be a remaining unknown
                    // field stored in `_next_unknown`.
                    while let Some((ordinal, data)) = _next_unknown.as_mut() {
                        let cur_offset: usize = (**ordinal as usize - 1) * 16;
                        // Zero reserved fields.
                        _encoder.padding(_offset + _prev_end_offset, cur_offset - _prev_end_offset);

                        // Safety:
                        // - bytes_len is calculated to fit 16*max(member.ordinal).
                        // - Since cur_offset is 16*(member.ordinal - 1) and the envelope takes
                        //   16 bytes, there is always sufficient room.
                        _unknown_encoder_func(data, _encoder, _offset + cur_offset, _recursion_depth)?;
                        _next_unknown = _unknown_fields.next();
                        _prev_end_offset = cur_offset + 16;
                    }

                    Ok(())
                })
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline(always)]
            fn new_empty() -> Self {
                Self::EMPTY
            }
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                // Decode envelope vector header
                let mut len: u64 = 0;
                let mut present: u64 = 0;
                len.decode(decoder, offset)?;
                present.decode(decoder, offset+8)?;

                match present {
                    $crate::encoding::ALLOC_PRESENT_U64 => (),
                    $crate::encoding::ALLOC_ABSENT_U64 => return Err($crate::Error::UnexpectedNullRef),
                    _ => return Err($crate::Error::InvalidPresenceIndicator),
                }

                let len = len as usize;
                let bytes_len = len * 16; // envelope inline_size is 16
                decoder.read_out_of_line(bytes_len, |decoder, offset| {
                    // Decode the envelope for each type.
                    // u32 num_bytes
                    // u32_num_handles
                    // 64-bit presence indicator
                    let mut _next_ordinal_to_read = 0;
                    let mut next_offset = offset;
                    let end_offset = offset + bytes_len;
                    $(
                        stringify!($value_unknown_name); // placeholder use for expansion
                        let mut _unknown_data = std::collections::BTreeMap::<u64, Vec<u8>>::new();
                        let _unknown_decoder_func = $crate::encoding::decode_unknown_bytes;
                    )?
                    $(
                        stringify!($resource_unknown_name); // placeholder use for expansion
                        let mut _unknown_data = std::collections::BTreeMap::<u64, $crate::encoding::UnknownData>::new();
                        let _unknown_decoder_func = $crate::encoding::decode_unknown_data;
                    )?
                    $(
                        _next_ordinal_to_read += 1;
                        if next_offset >= end_offset {
                            return Ok(());
                        }

                        // Decode unknown envelopes for gaps in ordinals.
                        while _next_ordinal_to_read < $ordinal {
                            if let Some(field) = _unknown_decoder_func(decoder, next_offset)? {
                                _unknown_data.insert(_next_ordinal_to_read, field);
                            }
                            _next_ordinal_to_read += 1;
                            next_offset += 16;
                        }

                        let mut num_bytes: u32 = 0;
                        num_bytes.decode(decoder, next_offset)?;
                        let mut num_handles: u32 = 0;
                        num_handles.decode(decoder, next_offset + 4)?;
                        let mut present: u64 = 0;
                        present.decode(decoder, next_offset + 8)?;
                        let next_out_of_line = decoder.next_out_of_line();
                        let handles_before = decoder.remaining_handles();
                        match present {
                            $crate::encoding::ALLOC_PRESENT_U64 => {
                                decoder.read_out_of_line(
                                    decoder.inline_size_of::<$member_ty>(),
                                    |decoder, offset| {
                                        $(
                                            decoder.set_next_handle_subtype($member_handle_subtype);
                                            decoder.set_next_handle_rights($member_handle_rights);
                                        )?
                                        let val_ref =
                                            self.$member_name.get_or_insert_with(
                                                || <$member_ty>::new_empty());
                                        val_ref.decode(decoder, offset)?;
                                        Ok(())
                                    },
                                )?;
                            }
                            $crate::encoding::ALLOC_ABSENT_U64 => {
                                if num_bytes != 0 {
                                    return Err($crate::Error::InvalidNumBytesInEnvelope);
                                }
                                if num_handles != 0 {
                                    return Err($crate::Error::InvalidNumHandlesInEnvelope);
                                }
                            }
                            _ => return Err($crate::Error::InvalidPresenceIndicator),
                        }
                        if decoder.next_out_of_line() != (next_out_of_line + (num_bytes as usize)) {
                            return Err($crate::Error::InvalidNumBytesInEnvelope);
                        }
                        if handles_before != (decoder.remaining_handles() + (num_handles as usize)) {
                            return Err($crate::Error::InvalidNumHandlesInEnvelope);
                        }

                        next_offset += 16;
                    )*

                    // Decode the remaining unknown envelopes.
                    while next_offset < end_offset {
                        _next_ordinal_to_read += 1;
                        if let Some(field) = _unknown_decoder_func(decoder, next_offset)? {
                            _unknown_data.insert(_next_ordinal_to_read, field);
                        }
                        next_offset += 16;
                    }

                    if !_unknown_data.is_empty() {
                        $(
                            self.$value_unknown_name = Some(_unknown_data);
                        )?
                        $(
                            self.$resource_unknown_name = Some(_unknown_data);
                        )?
                    }
                    Ok(())
                })
            }
        }
    }
}

/// Reverses the order of brace-enclosed statements.
///
/// Example:
///
/// ```rust
/// fidl_reverse_blocks! {
///     { println!("A"); }
///     { println!("B"); }
///     { println!("C"); }
/// }
/// ```
///
/// produces:
///
/// ```rust
/// { println!("C"); }
/// { println!("B"); }
/// { println!("A"); }
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! fidl_reverse_blocks {
    ($($b:block)*) => {
        $crate::fidl_reverse_blocks!(@internal { $($b)* } {})
    };
    (@internal { $head:block $($tail:block)* } { $($res:block)* }) => {
        $crate::fidl_reverse_blocks!(@internal { $($tail)* } { $head $($res)* })
    };
    (@internal {} { $($res:block)* }) => {
        #[allow(unused_braces)]
        { $($res)* }
    };
}

/// Decodes the inline portion of a xunion. Returns `(ordinal, num_bytes, num_handles)`.
#[inline]
pub fn decode_xunion_inline_portion(
    decoder: &mut Decoder,
    offset: usize,
) -> Result<(u64, u32, u32)> {
    let mut ordinal: u64 = 0;
    ordinal.decode(decoder, offset)?;

    let mut num_bytes: u32 = 0;
    num_bytes.decode(decoder, offset + 8)?;

    let mut num_handles: u32 = 0;
    num_handles.decode(decoder, offset + 12)?;

    let mut present: u64 = 0;
    present.decode(decoder, offset + 16)?;
    match present {
        ALLOC_PRESENT_U64 => (),
        ALLOC_ABSENT_U64 => {
            return Err(if num_bytes != 0 {
                Error::InvalidNumBytesInEnvelope
            } else if num_handles != 0 {
                Error::InvalidNumHandlesInEnvelope
            } else {
                Error::UnexpectedNullRef
            })
        }
        _ => return Err(Error::InvalidPresenceIndicator),
    }

    Ok((ordinal, num_bytes, num_handles))
}

impl<O, E> Layout for std::result::Result<O, E>
where
    O: Layout,
    E: Layout,
{
    #[inline(always)]
    fn inline_align(_context: &Context) -> usize {
        8
    }
    #[inline(always)]
    fn inline_size(_context: &Context) -> usize {
        24
    }
}

impl<O, E> Encodable for std::result::Result<O, E>
where
    O: Encodable,
    E: Encodable,
{
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        match self {
            Ok(val) => {
                // Encode success ordinal
                1u64.encode(encoder, offset, recursion_depth)?;
                // If the inline size is 0, meaning the type is (),
                // encode a zero byte instead because () in this context
                // means an empty struct, not an absent payload.
                if encoder.inline_size_of::<O>() == 0 {
                    encode_in_envelope(&mut Some(&mut 0u8), encoder, offset + 8, recursion_depth)
                } else {
                    encode_in_envelope(&mut Some(val), encoder, offset + 8, recursion_depth)
                }
            }
            Err(val) => {
                // Encode error ordinal
                2u64.encode(encoder, offset, recursion_depth)?;
                encode_in_envelope(&mut Some(val), encoder, offset + 8, recursion_depth)
            }
        }
    }
}

impl<O, E> Decodable for std::result::Result<O, E>
where
    O: Decodable,
    E: Decodable,
{
    #[inline(always)]
    fn new_empty() -> Self {
        Ok(<O as Decodable>::new_empty())
    }

    #[inline]
    fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
        decoder.debug_check_bounds::<Self>(offset);
        let (ordinal, _, _) = decode_xunion_inline_portion(decoder, offset)?;
        let member_inline_size = match ordinal {
            1 => {
                // If the inline size is 0, meaning the type is (), use an inline
                // size of 1 instead because () in this context means an empty
                // struct, not an absent payload.
                cmp::max(1, decoder.inline_size_of::<O>())
            }
            2 => decoder.inline_size_of::<E>(),
            _ => return Err(Error::UnknownUnionTag),
        };
        decoder.read_out_of_line(member_inline_size, |decoder, offset| {
            match ordinal {
                1 => {
                    if let Ok(_) = self {
                        // Do nothing, read the value into the object
                    } else {
                        // Initialize `self` to the right variant
                        *self = Ok(O::new_empty());
                    }
                    if let Ok(val) = self {
                        // If the inline size is 0, then the type is ().
                        // () has a different wire-format representation in
                        // a result vs outside of a result, so special case
                        // decode.
                        if decoder.inline_size_of::<O>() == 0 {
                            decoder.check_padding(offset, 1)
                        } else {
                            val.decode(decoder, offset)
                        }
                    } else {
                        unreachable!()
                    }
                }
                2 => {
                    if let Err(_) = self {
                        // Do nothing, read the value into the object
                    } else {
                        // Initialize `self` to the right variant
                        *self = Err(E::new_empty());
                    }
                    if let Err(val) = self {
                        val.decode(decoder, offset)
                    } else {
                        unreachable!()
                    }
                }
                // Should be unreachable, since we already checked above.
                ordinal => panic!("unexpected ordinal {:?}", ordinal),
            }
        })
    }
}

/// Implements the FIDL `Encodable` and `Decodable` traits for an enum
/// representing a FIDL union.
#[macro_export]
macro_rules! fidl_union {
    (
        name: $name:ident,
        members: [$(
            $member_name:ident {
                ty: $member_ty:ty,
                ordinal: $member_ordinal:expr,
                $(
                    handle_metadata: {
                        handle_subtype: $member_handle_subtype:expr,
                        handle_rights: $member_handle_rights:expr,
                    },
                )?
            },
        )*],
        // Flexible xunions only: provide the name of the unknown variant using
        // either `resource_unknown_member` or `value_unknown_member`.
        $( resource_unknown_member: $resource_unknown_name:ident, )?
        $( value_unknown_member: $value_unknown_name:ident, )?
    ) => {
        impl $name {
            #[inline]
            fn ordinal(&self) -> u64 {
                match *self {
                    $(
                        $name::$member_name(_) => $member_ordinal,
                    )*
                    $(
                        #[allow(deprecated)]
                        $name::$value_unknown_name { ordinal, .. } => ordinal,
                    )?
                    $(
                        #[allow(deprecated)]
                        $name::$resource_unknown_name { ordinal, .. } => ordinal,
                    )?
                }
            }
        }

        impl $crate::encoding::Layout for $name {
            #[inline(always)]
            fn inline_align(_context: &$crate::encoding::Context) -> usize { 8 }
            #[inline(always)]
            fn inline_size(_context: &$crate::encoding::Context) -> usize { 24 }
        }

        impl $crate::encoding::Encodable for $name {
            #[inline]
            fn encode(&mut self, encoder: &mut $crate::encoding::Encoder<'_, '_>, offset: usize, recursion_depth: usize) -> $crate::Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                let mut ordinal = self.ordinal();
                // Encode ordinal
                ordinal.encode(encoder, offset, recursion_depth)?;
                match self {
                    $(
                        $name::$member_name ( val ) => {
                            $(
                                encoder.set_next_handle_subtype($member_handle_subtype);
                                encoder.set_next_handle_rights($member_handle_rights);
                            )?
                            $crate::encoding::encode_in_envelope(&mut Some(val), encoder, offset+8, recursion_depth)
                        },
                    )*
                    $(
                        #[allow(deprecated)]
                        $name::$resource_unknown_name { ordinal: _, data } => {
                            // Throw the raw data from the unrecognized variant
                            // back onto the wire. This will allow correct proxies even in
                            // the event that they don't yet recognize this union variant.
                            $crate::encoding::encode_unknown_data(data, encoder, offset + 8, recursion_depth)
                        },
                    )?
                    $(
                        #[allow(deprecated)]
                        $name::$value_unknown_name { ordinal: _, bytes } => {
                            // Throw the raw data from the unrecognized variant
                            // back onto the wire. This will allow correct proxies even in
                            // the event that they don't yet recognize this union variant.
                            $crate::encoding::encode_unknown_bytes(&bytes, encoder, offset + 8, recursion_depth)
                        },
                    )?
                }
            }
        }

        impl $crate::encoding::Decodable for $name {
            #[inline(always)]
            fn new_empty() -> Self {
                #![allow(unreachable_code)]
                $(
                    return $name::$member_name(<$member_ty>::new_empty());
                )*
                $(
                    #[allow(deprecated)]
                    $name::$resource_unknown_name {
                        ordinal: 0,
                        data: $crate::encoding::UnknownData { bytes: vec![], handles: vec![] }
                    }
                )?
                $(
                    #[allow(deprecated)]
                    $name::$value_unknown_name { ordinal: 0, bytes: vec![] }
                )?
            }

            #[inline]
            fn decode(&mut self, decoder: &mut $crate::encoding::Decoder<'_>, offset: usize) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                #[allow(unused_variables)]
                let next_out_of_line = decoder.next_out_of_line();
                let handles_before = decoder.remaining_handles();
                let (ordinal, num_bytes, num_handles) = $crate::encoding::decode_xunion_inline_portion(decoder, offset)?;
                let member_inline_size = match ordinal {
                    $(
                        $member_ordinal => decoder.inline_size_of::<$member_ty>(),
                    )*
                    $(
                        #[allow(deprecated)]
                        _ => {
                            stringify!($resource_unknown_name); // placeholder use for expansion
                            // Flexible xunion: unknown payloads are considered
                            // a wholly-inline string of bytes.
                            num_bytes as usize
                        }
                    )?
                    $(
                        #[allow(deprecated)]
                        _ => {
                            // Disallow unknown handles in non-resource types.
                            if (num_handles > 0) {
                                for _ in 0..num_handles {
                                    decoder.drop_next_handle()?;
                                }
                                return Err($crate::Error::CannotStoreUnknownHandles);
                            }
                            stringify!($value_unknown_name); // placeholder use for expansion
                            // Flexible xunion: unknown payloads are considered
                            // a wholly-inline string of bytes.
                            num_bytes as usize
                        }
                    )?
                    // Strict xunion: reject unknown ordinals.
                    #[allow(unreachable_patterns)]
                    _ => {
                        for _ in 0..num_handles {
                            decoder.drop_next_handle()?;
                        }
                        return Err($crate::Error::UnknownUnionTag);
                    },
                };

                decoder.read_out_of_line(member_inline_size, |decoder, offset| {
                    match ordinal {
                        $(
                            $member_ordinal => {
                                $(
                                    decoder.set_next_handle_subtype($member_handle_subtype);
                                    decoder.set_next_handle_rights($member_handle_rights);
                                )?
                                #[allow(irrefutable_let_patterns)]
                                if let $name::$member_name(_) = self {
                                    // Do nothing, read the value into the object
                                } else {
                                    // Initialize `self` to the right variant
                                    *self = $name::$member_name(
                                        <$member_ty>::new_empty()
                                    );
                                }
                                #[allow(irrefutable_let_patterns)]
                                if let $name::$member_name(val) = self {
                                    val.decode(decoder, offset)?;
                                } else {
                                    unreachable!()
                                }
                            }
                        )*
                        $(
                            #[allow(deprecated)]
                            ordinal => {
                                let data = $crate::encoding::decode_unknown_data_contents(decoder, offset, num_bytes, num_handles)?;
                                *self = $name::$resource_unknown_name { ordinal, data };
                            }
                        )?
                        $(
                            #[allow(deprecated)]
                            ordinal => {
                                let bytes = decoder.buffer()[offset.. offset+(num_bytes as usize)].to_vec();
                                *self = $name::$value_unknown_name { ordinal, bytes };
                            }
                        )?
                        // This should be unreachable, since we already
                        // checked for unknown ordinals above and returned
                        // an error in the strict case.
                        #[allow(unreachable_patterns)]
                        ordinal => panic!("unexpected ordinal {:?}", ordinal)
                    }
                    Ok(())
                })?;

                if handles_before != (decoder.remaining_handles() + (num_handles as usize)) {
                    return Err($crate::Error::InvalidNumHandlesInEnvelope);
                }
                if decoder.next_out_of_line() != (next_out_of_line + (num_bytes as usize)) {
                    return Err($crate::Error::InvalidNumBytesInEnvelope);
                }
                Ok(())
            }
        }

        impl $crate::encoding::Autonull for $name {
            #[inline(always)]
            fn naturally_nullable(_context: &$crate::encoding::Context) -> bool {
                true
            }
        }
    }
}

/// Container for the raw bytes and handles of an unknown envelope payload.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct UnknownData {
    /// Unknown bytes.
    pub bytes: Vec<u8>,
    /// Unknown handles.
    pub handles: Vec<Handle>,
}

/// Header for transactional FIDL messages
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct TransactionHeader {
    /// Transaction ID which identifies a request-response pair
    tx_id: u32,
    /// Flags set for this message. MUST NOT be validated by bindings
    flags: [u8; 3],
    /// Magic number indicating the message's wire format. Two sides with
    /// different magic numbers are incompatible with each other.
    magic_number: u8,
    /// Ordinal which identifies the FIDL method
    ordinal: u64,
}

impl TransactionHeader {
    /// Returns whether the message containing this TransactionHeader is in a
    /// compatible wire format
    #[inline]
    pub fn is_compatible(&self) -> bool {
        self.magic_number == MAGIC_NUMBER_INITIAL
    }
}

fidl_struct_copy! {
    name: TransactionHeader,
    members: [
        tx_id {
            ty: u32,
            offset_v1: 0,
        },
        flags {
            ty: [u8; 3],
            offset_v1: 4,
        },
        magic_number {
            ty: u8,
            offset_v1: 7,
        },
        ordinal {
            ty: u64,
            offset_v1: 8,
        },
    ],
    padding: [],
    size_v1: 16,
    align_v1: 8,
}

bitflags! {
    /// Bitflags type for transaction header flags.
    pub struct HeaderFlags: u32 {
        /// Empty placeholder since empty bitflags are not allowed. Should be
        /// removed once any new header flags are defined.
        #[deprecated = "Placeholder since empty bitflags are not allowed."]
        const __PLACEHOLDER = 0;
    }
}

impl Into<[u8; 3]> for HeaderFlags {
    #[inline]
    fn into(self) -> [u8; 3] {
        let bytes = self.bits.to_le_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }
}

impl TransactionHeader {
    /// Creates a new transaction header with the default encode context and magic number.
    #[inline]
    pub fn new(tx_id: u32, ordinal: u64) -> Self {
        TransactionHeader::new_full(tx_id, ordinal, &default_encode_context(), MAGIC_NUMBER_INITIAL)
    }
    /// Creates a new transaction header with a specific context and magic number.
    #[inline]
    pub fn new_full(tx_id: u32, ordinal: u64, context: &Context, magic_number: u8) -> Self {
        TransactionHeader { tx_id, flags: context.header_flags().into(), magic_number, ordinal }
    }
    /// Returns the header's transaction id.
    #[inline]
    pub fn tx_id(&self) -> u32 {
        self.tx_id
    }
    /// Returns the header's message ordinal.
    #[inline]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
    /// Returns true if the header is for an epitaph message.
    #[inline]
    pub fn is_epitaph(&self) -> bool {
        self.ordinal == EPITAPH_ORDINAL
    }

    /// Returns the magic number.
    #[inline]
    pub fn magic_number(&self) -> u8 {
        self.magic_number
    }

    /// Returns the header's flags as a `HeaderFlags` value.
    #[inline]
    pub fn flags(&self) -> HeaderFlags {
        let bytes = [self.flags[0], self.flags[1], self.flags[2], 0];
        HeaderFlags::from_bits_truncate(u32::from_le_bytes(bytes))
    }

    /// Returns the context to use for decoding the message body associated with
    /// this header. During migrations, this is dependent on `self.flags()` and
    /// controls dynamic behavior in the read path.
    #[inline]
    pub fn decoding_context(&self) -> Context {
        Context {}
    }
}

/// Transactional FIDL message
pub struct TransactionMessage<'a, T> {
    /// Header of the message
    pub header: TransactionHeader,
    /// Body of the message
    pub body: &'a mut T,
}

impl<T: Layout> Layout for TransactionMessage<'_, T> {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        cmp::max(<TransactionHeader as Layout>::inline_align(context), T::inline_align(context))
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        <TransactionHeader as Layout>::inline_size(context) + T::inline_size(context)
    }
}

impl<T: Encodable> Encodable for TransactionMessage<'_, T> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        self.header.encode(encoder, offset, recursion_depth)?;
        (*self.body).encode(
            encoder,
            offset + encoder.inline_size_of::<TransactionHeader>(),
            recursion_depth,
        )?;
        Ok(())
    }
}

// To decode TransactionMessage<MyObject>, use this pattern:
//
//     let (header, body_bytes) = decode_transaction_header(bytes)?;
//     let mut my_object = MyObject::new_empty();
//     Decoder::decode_into(&header, body_bytes, handles, &mut my_object)?;
//
// We _could_ implement Decodable for TransactionMessage<T>, but it would only
// work when you know the type T upfront, which is often not the case (for
// example, it might depend on the ordinal). To avoid having two code paths that
// could get out of sync, we simply do not implement Decodable.
assert_not_impl_any!(TransactionMessage<()>: Decodable);

/// Decodes the transaction header from a message.
/// Returns the header and a reference to the tail of the message.
pub fn decode_transaction_header(bytes: &[u8]) -> Result<(TransactionHeader, &[u8])> {
    let mut header = TransactionHeader::new_empty();
    let context = Context {};
    let header_len = <TransactionHeader as Layout>::inline_size(&context);
    if bytes.len() < header_len {
        return Err(Error::OutOfRange);
    }
    let (header_bytes, body_bytes) = bytes.split_at(header_len);
    let handles = &mut [];
    Decoder::decode_with_context(&context, header_bytes, handles, &mut header)?;
    Ok((header, body_bytes))
}

/// Header for persistently stored FIDL messages
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PersistentHeader {
    /// Flags set for this message. MUST NOT be validated by bindings
    flags: [u8; 3],
    /// Magic number indicating the message's wire format. Two sides with
    /// different magic numbers are incompatible with each other.
    magic_number: u8,
}

fidl_struct! {
    name: PersistentHeader,
    members: [
        flags {
            ty: [u8; 3],
            offset_v1: 4,
        },
        magic_number {
            ty: u8,
            offset_v1: 7,
        },
    ],
    padding: [
        {
            ty: u32,
            offset: 0,
            mask: 0xffffffffu32,
        },
        {
            ty: u64,
            offset: 8,
            mask: 0xffffffffffffffffu64,
        },
    ],
    size_v1: 16,
    align_v1: 8,
}

impl PersistentHeader {
    /// Creates a new `PersistentHeader` with the default encode context and magic number.
    #[inline]
    pub fn new() -> Self {
        PersistentHeader::new_full(&default_encode_context(), MAGIC_NUMBER_INITIAL)
    }
    /// Creates a new `PersistentHeader` with a specific context and magic number.
    #[inline]
    pub fn new_full(context: &Context, magic_number: u8) -> Self {
        PersistentHeader { flags: context.header_flags().into(), magic_number }
    }
    /// Returns the magic number.
    #[inline]
    pub fn magic_number(&self) -> u8 {
        self.magic_number
    }
    /// Returns the header's flags as a `HeaderFlags` value.
    #[inline]
    pub fn flags(&self) -> HeaderFlags {
        let bytes = [self.flags[0], self.flags[1], self.flags[2], 0];
        HeaderFlags::from_bits_truncate(u32::from_le_bytes(bytes))
    }
    /// Returns the context to use for decoding the message body associated with
    /// this header. During migrations, this is dependent on `self.flags()` and
    /// controls dynamic behavior in the read path.
    #[inline]
    pub fn decoding_context(&self) -> Context {
        Context {}
    }
    /// Returns whether the message containing this `PersistentHeader` is in a
    /// compatible wire format.
    #[inline]
    pub fn is_compatible(&self) -> bool {
        self.magic_number == MAGIC_NUMBER_INITIAL
    }
}

/// Persistently stored FIDL message
pub struct PersistentMessage<'a, T> {
    /// Header of the message
    pub header: PersistentHeader,
    /// Body of the message
    pub body: &'a mut T,
}

impl<T: Layout> Layout for PersistentMessage<'_, T> {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        cmp::max(<PersistentHeader as Layout>::inline_align(context), T::inline_align(context))
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        <PersistentHeader as Layout>::inline_size(context) + T::inline_size(context)
    }
}

impl<T: Encodable> Encodable for PersistentMessage<'_, T> {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        encoder.debug_check_bounds::<Self>(offset);
        self.header.encode(encoder, offset, recursion_depth)?;
        (*self.body).encode(
            encoder,
            offset + encoder.inline_size_of::<PersistentHeader>(),
            recursion_depth,
        )?;
        Ok(())
    }
}

/// Encode the referred parameter into persistent binary form.
/// Generates and adds message header to the returned bytes.
pub fn encode_persistent<T: Encodable>(body: &mut T) -> Result<Vec<u8>> {
    let msg = &mut PersistentMessage { header: PersistentHeader::new(), body };
    let mut combined_bytes = Vec::<u8>::new();
    let mut handles = Vec::<HandleDisposition<'static>>::new();
    Encoder::encode(&mut combined_bytes, &mut handles, msg)?;
    debug_assert!(handles.is_empty(), "Persistent message contains handles");
    Ok(combined_bytes)
}

/// Creates persistent header to encode it and the message body separately.
pub fn create_persistent_header() -> PersistentHeader {
    PersistentHeader::new()
}

/// Encode PersistentHeader to persistent binary form.
pub fn encode_persistent_header(header: &mut PersistentHeader) -> Result<Vec<u8>> {
    let mut header_bytes = Vec::<u8>::new();
    Encoder::encode(&mut header_bytes, &mut Vec::new(), header)?;
    Ok(header_bytes)
}

/// Encode the message body to to persistent binary form.
pub fn encode_persistent_body<T: Encodable>(
    body: &mut T,
    header: &PersistentHeader,
) -> Result<Vec<u8>> {
    let mut combined_bytes = Vec::<u8>::new();
    let mut handles = Vec::<HandleDisposition<'static>>::new();
    Encoder::encode_with_context(
        &header.decoding_context(),
        &mut combined_bytes,
        &mut handles,
        body,
    )?;
    debug_assert!(handles.is_empty(), "Persistent message contains handles");
    Ok(combined_bytes)
}

/// Decode the type expected from the persistent binary form.
pub fn decode_persistent<T: Decodable>(bytes: &[u8]) -> Result<T> {
    let context = Context {};
    let header_len = <PersistentHeader as Layout>::inline_size(&context);
    if bytes.len() < header_len {
        return Err(Error::OutOfRange);
    }
    let (header_bytes, body_bytes) = bytes.split_at(header_len);
    let header = decode_persistent_header(header_bytes)?;
    decode_persistent_body(body_bytes, &header)
}

/// Decodes the persistently stored header from a message.
/// Returns the header and a reference to the tail of the message.
pub fn decode_persistent_header(bytes: &[u8]) -> Result<PersistentHeader> {
    let mut header = PersistentHeader::new_empty();
    Decoder::decode_with_context(&header.decoding_context(), bytes, &mut [], &mut header)?;
    Ok(header)
}

/// Decodes the persistently stored header from a message.
/// Returns the header and a reference to the tail of the message.
pub fn decode_persistent_body<T: Decodable>(bytes: &[u8], header: &PersistentHeader) -> Result<T> {
    let mut output = T::new_empty();
    Decoder::decode_with_context(&header.decoding_context(), bytes, &mut [], &mut output)?;
    Ok(output)
}

/// Creates a type that wraps a value and provides object type and rights information.
#[macro_export]
macro_rules! wrap_handle_metadata {
    ($name:ident, $object_type:expr, $rights:expr) => {
        pub struct $name<T>(T);

        impl<T> $name<T> {
            pub fn into_inner(self) -> T {
                self.0
            }
        }

        impl<T: $crate::encoding::Layout> $crate::encoding::Layout for $name<T> {
            #[inline]
            fn inline_align(context: &$crate::encoding::Context) -> usize {
                T::inline_align(context)
            }

            #[inline]
            fn inline_size(context: &$crate::encoding::Context) -> usize {
                T::inline_size(context)
            }
        }

        impl<T: $crate::encoding::Encodable> $crate::encoding::Encodable for $name<T> {
            #[inline]
            fn encode(
                &mut self,
                encoder: &mut $crate::encoding::Encoder<'_, '_>,
                offset: usize,
                recursion_depth: usize,
            ) -> $crate::Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                encoder.set_next_handle_subtype($object_type);
                encoder.set_next_handle_rights($rights);
                self.0.encode(encoder, offset, recursion_depth)
            }
        }

        impl<T: $crate::encoding::Decodable> $crate::encoding::Decodable for $name<T> {
            #[inline]
            fn new_empty() -> Self {
                Self(T::new_empty())
            }

            #[inline]
            fn decode(
                &mut self,
                decoder: &mut $crate::encoding::Decoder<'_>,
                offset: usize,
            ) -> $crate::Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                decoder.set_next_handle_subtype($object_type);
                decoder.set_next_handle_rights($rights);
                self.0.decode(decoder, offset)
            }
        }
    };
}

// Implementations of Encodable for (&mut Head, ...Tail) and Decodable for (Head, ...Tail)
macro_rules! tuple_impls {
    () => {};

    (($idx:tt => $typ:ident), $( ($nidx:tt => $ntyp:ident), )*) => {
        /*
         * Invoke recursive reversal of list that ends in the macro expansion implementation
         * of the reversed list
        */
        tuple_impls!([($idx, $typ);] $( ($nidx => $ntyp), )*);
        tuple_impls!($( ($nidx => $ntyp), )*); // invoke macro on tail
    };

    /*
     * ([accumulatedList], listToReverse); recursively calls tuple_impls until the list to reverse
     + is empty (see next pattern)
    */
    ([$(($accIdx:tt, $accTyp:ident);)+]
        ($idx:tt => $typ:ident), $( ($nidx:tt => $ntyp:ident), )*) => {
      tuple_impls!([($idx, $typ); $(($accIdx, $accTyp); )*] $( ($nidx => $ntyp), ) *);
    };

    // Finally expand into the implementation
    ([($idx:tt, $typ:ident); $( ($nidx:tt, $ntyp:ident); )*]) => {
        impl<$typ, $( $ntyp ),*> Layout for ($typ, $( $ntyp, )*)
            where $typ: Layout,
                  $( $ntyp: Layout, )*
        {
            #[inline(always)]
            fn inline_align(context: &Context) -> usize {
                let mut max = 0;
                if max < $typ::inline_align(context) {
                    max = $typ::inline_align(context);
                }
                $(
                    if max < $ntyp::inline_align(context) {
                        max = $ntyp::inline_align(context);
                    }
                )*
                max
            }

            #[inline(always)]
            fn inline_size(context: &Context) -> usize {
                let mut offset = 0;
                offset += $typ::inline_size(context);
                $(
                    offset = round_up_to_align(offset, $ntyp::inline_align(context));
                    offset += $ntyp::inline_size(context);
                )*
                offset
            }
        }

        impl<$typ, $( $ntyp ,)*> Encodable for ($typ, $( $ntyp ,)*)
            where $typ: Encodable,
                  $( $ntyp: Encodable,)*
        {
            #[inline]
            fn encode(&mut self, encoder: &mut Encoder<'_, '_>, offset: usize, recursion_depth: usize) -> Result<()> {
                encoder.debug_check_bounds::<Self>(offset);
                // Tuples are encoded like structs.
                // $idx is always 0 for the first element.
                self.$idx.encode(encoder, offset, recursion_depth)?;
                let mut cur_offset = 0;
                cur_offset += encoder.inline_size_of::<$typ>();
                $(
                    // Skip to the start of the next field
                    let member_offset =
                        round_up_to_align(cur_offset, encoder.inline_align_of::<$ntyp>());
                    encoder.padding(offset + cur_offset, member_offset - cur_offset);
                    self.$nidx.encode(encoder, offset + member_offset, recursion_depth)?;
                    cur_offset = member_offset + encoder.inline_size_of::<$ntyp>();
                )*
                encoder.padding(offset + cur_offset, encoder.inline_size_of::<Self>() - cur_offset);
                Ok(())
            }
        }

        impl<$typ, $( $ntyp ),*> Decodable for ($typ, $( $ntyp, )*)
            where $typ: Decodable,
                  $( $ntyp: Decodable, )*
        {
            #[inline]
            fn new_empty() -> Self {
                (
                    $typ::new_empty(),
                    $(
                        $ntyp::new_empty(),
                    )*
                )
            }

            #[inline]
            fn decode(&mut self, decoder: &mut Decoder<'_>, offset: usize) -> Result<()> {
                decoder.debug_check_bounds::<Self>(offset);
                self.$idx.decode(decoder, offset)?;
                let mut _cur_offset = decoder.inline_size_of::<$typ>();
                $(
                    // Skip to the start of the next field
                    let member_offset =
                        round_up_to_align(_cur_offset, decoder.inline_align_of::<$ntyp>());
                    decoder.check_padding(offset + _cur_offset, member_offset - _cur_offset)?;
                    self.$nidx.decode(decoder, offset + member_offset)?;
                    _cur_offset = member_offset + decoder.inline_size_of::<$ntyp>();
                )*
                // Skip to the end of the struct's size
                decoder.check_padding(offset + _cur_offset, decoder.inline_size_of::<Self>() - _cur_offset)?;
                Ok(())
            }
        }
    }
}

tuple_impls!(
    (10 => K),
    (9 => J),
    (8 => I),
    (7 => H),
    (6 => G),
    (5 => F),
    (4 => E),
    (3 => D),
    (2 => C),
    (1 => B),
    (0 => A),
);

// The unit type has 0 size because it represents the absent payload after the
// transaction header in the reponse of a two-way FIDL method such as this one:
//
//     Method() -> ();
//
// However, the unit type is also used in the following situation:
//
//    MethodWithError() -> () error int32;
//
// In this case the response type is std::result::Result<(), i32>, but the ()
// represents an empty struct, which has size 1. To accommodate this, the encode
// and decode methods on std::result::Result handle the () case specially.
impl_layout!((), align: 1, size: 0);

impl Encodable for () {
    #[inline]
    fn encode(
        &mut self,
        _: &mut Encoder<'_, '_>,
        _offset: usize,
        _recursion_depth: usize,
    ) -> Result<()> {
        Ok(())
    }
}

impl Decodable for () {
    #[inline(always)]
    fn new_empty() -> Self {
        ()
    }
    #[inline]
    fn decode(&mut self, _: &mut Decoder<'_>, _offset: usize) -> Result<()> {
        Ok(())
    }
}

impl<T: Layout> Layout for &mut T {
    #[inline(always)]
    fn inline_align(context: &Context) -> usize {
        T::inline_align(context)
    }
    #[inline(always)]
    fn inline_size(context: &Context) -> usize {
        T::inline_size(context)
    }
}

impl<T: Encodable> Encodable for &mut T {
    #[inline]
    fn encode(
        &mut self,
        encoder: &mut Encoder<'_, '_>,
        offset: usize,
        recursion_depth: usize,
    ) -> Result<()> {
        (&mut **self).encode(encoder, offset, recursion_depth)
    }
}

#[cfg(test)]
mod test {
    // Silence dead code errors from unused functions produced by macros like
    // `fidl_bits!`, `fidl_union!`, etc. To the compiler, it's as if we defined
    // a pub fn in a private mod and never used it. Unfortunately placing this
    // attribute directly on the macro invocations does not work.
    #![allow(dead_code)]

    use super::*;
    use matches::assert_matches;
    use std::{collections::BTreeMap, f32, f64, fmt, i64, u64};

    pub const CONTEXTS: &[&Context] = &[&Context {}];

    fn to_handle_info(handles: &mut Vec<HandleDisposition<'static>>) -> Vec<HandleInfo> {
        handles
            .drain(..)
            .map(|h| {
                assert_eq!(h.result, Status::OK);
                if let HandleOp::Move(mut handle) = h.handle_op {
                    return HandleInfo {
                        handle: mem::replace(&mut handle, Handle::invalid()),
                        object_type: h.object_type,
                        rights: h.rights,
                    };
                }
                panic!("expected HandleOp::Move");
            })
            .collect()
    }

    #[track_caller]
    pub fn encode_decode<T: Encodable + Decodable>(ctx: &Context, start: &mut T) -> T {
        let buf = &mut Vec::new();
        let handle_buf = &mut Vec::new();
        Encoder::encode_with_context(ctx, buf, handle_buf, start).expect("Encoding failed");
        let mut out = T::new_empty();
        Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
            .expect("Decoding failed");
        out
    }

    #[track_caller]
    fn encode_assert_bytes<T: Encodable>(ctx: &Context, mut data: T, encoded_bytes: &[u8]) {
        let buf = &mut Vec::new();
        let handle_buf = &mut Vec::new();
        Encoder::encode_with_context(ctx, buf, handle_buf, &mut data).expect("Encoding failed");
        assert_eq!(&**buf, encoded_bytes);
    }

    #[track_caller]
    fn assert_identity<T>(mut x: T, cloned: T)
    where
        T: Encodable + Decodable + PartialEq + fmt::Debug,
    {
        for ctx in CONTEXTS {
            assert_eq!(cloned, encode_decode(ctx, &mut x));
        }
    }

    macro_rules! identities { ($($x:expr,)*) => { $(
        assert_identity($x, $x);
    )* } }

    #[test]
    fn encode_decode_byte() {
        identities![0u8, 57u8, 255u8, 0i8, -57i8, 12i8,];
    }

    #[test]
    #[rustfmt::skip]
    fn encode_decode_multibyte() {
        identities![
            0u64, 1u64, u64::MAX, u64::MIN,
            0i64, 1i64, i64::MAX, i64::MIN,
            0f32, 1f32, f32::MAX, f32::MIN,
            0f64, 1f64, f64::MAX, f64::MIN,
        ];
    }

    #[test]
    fn encode_decode_nan() {
        for ctx in CONTEXTS {
            let nan32 = encode_decode(ctx, &mut f32::NAN.clone());
            assert!(nan32.is_nan());

            let nan64 = encode_decode(ctx, &mut f64::NAN.clone());
            assert!(nan64.is_nan());
        }
    }

    #[test]
    fn encode_decode_out_of_line() {
        identities![
            Vec::<i32>::new(),
            vec![1, 2, 3],
            None::<Vec<i32>>,
            Some(Vec::<i32>::new()),
            Some(vec![1, 2, 3]),
            Some(vec![vec![1, 2, 3]]),
            Some(vec![Some(vec![1, 2, 3])]),
            "".to_string(),
            "foo".to_string(),
            None::<String>,
            Some("".to_string()),
            Some("foo".to_string()),
            Some(vec![None, Some("foo".to_string())]),
            vec!["foo".to_string(), "bar".to_string()],
        ];
    }

    pub fn assert_identity_slice<'a, T>(ctx: &Context, mut start: &'a [T])
    where
        &'a [T]: Encodable,
        Vec<T>: Decodable,
        T: PartialEq + fmt::Debug,
    {
        let buf = &mut Vec::new();
        let handle_buf = &mut Vec::new();
        Encoder::encode_with_context(ctx, buf, handle_buf, &mut start).expect("Encoding failed");
        let mut out = Vec::<T>::new_empty();
        Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
            .expect("Decoding failed");
        assert_eq!(start, &out[..]);
    }

    #[test]
    fn encode_slices_of_primitives() {
        for ctx in CONTEXTS {
            assert_identity_slice(ctx, &[] as &[u8]);
            assert_identity_slice(ctx, &[0u8]);
            assert_identity_slice(ctx, &[1u8, 2, 3, 4, 5, 255]);

            assert_identity_slice(ctx, &[] as &[i8]);
            assert_identity_slice(ctx, &[0i8]);
            assert_identity_slice(ctx, &[1i8, 2, 3, 4, 5, -128, 127]);

            assert_identity_slice(ctx, &[] as &[u64]);
            assert_identity_slice(ctx, &[0u64]);
            assert_identity_slice(ctx, &[1u64, 2, 3, 4, 5, u64::MAX]);

            assert_identity_slice(ctx, &[] as &[f32]);
            assert_identity_slice(ctx, &[0.0f32]);
            assert_identity_slice(ctx, &[1.0f32, 2.0, 3.0, 4.0, 5.0, f32::MIN, f32::MAX]);

            assert_identity_slice(ctx, &[] as &[f64]);
            assert_identity_slice(ctx, &[0.0f64]);
            assert_identity_slice(ctx, &[1.0f64, 2.0, 3.0, 4.0, 5.0, f64::MIN, f64::MAX]);
        }
    }

    #[test]
    fn result_encode_empty_ok_value() {
        identities![(), Ok::<(), i32>(()),];
        for ctx in CONTEXTS {
            // An empty response is represented by () and has zero size.
            encode_assert_bytes(ctx, (), &[]);
            // But in the context of an error result type Result<(), ErrorType>, the
            // () in Ok(()) is treated as an empty struct (with size 1).
            encode_assert_bytes(
                ctx,
                Ok::<(), i32>(()),
                &[
                    0x01, 0x00, 0x00, 0x00, // success ordinal
                    0x00, 0x00, 0x00, 0x00, // success ordinal [cont.]
                    0x08, 0x00, 0x00, 0x00, // 8 bytes (rounded up from 1)
                    0x00, 0x00, 0x00, 0x00, // 0 handles
                    0xff, 0xff, 0xff, 0xff, // present
                    0xff, 0xff, 0xff, 0xff, // present [cont.]
                    0x00, 0x00, 0x00, 0x00, // empty struct + 3 bytes padding
                    0x00, 0x00, 0x00, 0x00, // padding
                ],
            );
        }
    }

    #[test]
    fn result_with_size_non_multiple_of_align() {
        type Res = std::result::Result<(Vec<u8>, bool), u32>;

        identities![
            Res::Ok((vec![], true)),
            Res::Ok((vec![], false)),
            Res::Ok((vec![1, 2, 3, 4, 5], true)),
            Res::Err(7u32),
        ];
    }

    #[test]
    fn result_and_xunion_compat() {
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        enum OkayOrError {
            Okay(u64),
            Error(u32),
        }
        fidl_union! {
            name: OkayOrError,
            members: [
                Okay {
                    ty: u64,
                    ordinal: 1,
                },
                Error {
                    ty: u32,
                    ordinal: 2,
                },
            ],
        };

        for ctx in CONTEXTS {
            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            let mut out: std::result::Result<u64, u32> = Decodable::new_empty();

            Encoder::encode_with_context(ctx, buf, handle_buf, &mut OkayOrError::Okay(42u64))
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(out, Ok(42));

            Encoder::encode_with_context(ctx, buf, handle_buf, &mut OkayOrError::Error(3u32))
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(out, Err(3));
        }
    }

    #[test]
    fn result_and_xunion_compat_smaller() {
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        pub struct Empty;
        fidl_empty_struct!(Empty);
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        enum OkayOrError {
            Okay(Empty),
            Error(i32),
        }
        fidl_union! {
            name: OkayOrError,
            members: [
                Okay {
                    ty: Empty,
                    ordinal: 1,
                },
                Error {
                    ty: i32,
                    ordinal: 2,
                },
            ],
        };

        for ctx in CONTEXTS {
            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();

            // result to xunion
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut Ok::<(), i32>(()))
                .expect("Encoding failed");
            let mut out = OkayOrError::new_empty();
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(out, OkayOrError::Okay(Empty {}));

            Encoder::encode_with_context(ctx, buf, handle_buf, &mut Err::<(), i32>(5))
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(out, OkayOrError::Error(5));

            // xunion to result
            let mut out: std::result::Result<(), i32> = Decodable::new_empty();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut OkayOrError::Okay(Empty {}))
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(out, Ok(()));

            Encoder::encode_with_context(ctx, buf, handle_buf, &mut OkayOrError::Error(3i32))
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(out, Err(3));
        }
    }

    #[test]
    fn encode_decode_result() {
        for ctx in CONTEXTS {
            let mut test_result: std::result::Result<String, u32> = Ok("fuchsia".to_string());
            let mut test_result_err: std::result::Result<String, u32> = Err(5);

            match encode_decode(ctx, &mut test_result) {
                Ok(ref out_str) if "fuchsia".to_string() == *out_str => {}
                x => panic!("unexpected decoded value {:?}", x),
            }

            match &encode_decode(ctx, &mut test_result_err) {
                Err(err_code) if *err_code == 5 => {}
                x => panic!("unexpected decoded value {:?}", x),
            }
        }
    }

    #[test]
    fn encode_decode_result_array() {
        use std::result::Result;

        for ctx in CONTEXTS {
            {
                let mut input: [Result<_, u32>; 2] = [Ok("a".to_string()), Ok("bcd".to_string())];
                match encode_decode(ctx, &mut input) {
                    [Ok(ref ok1), Ok(ref ok2)]
                        if *ok1 == "a".to_string() && *ok2 == "bcd".to_string() => {}
                    x => panic!("unexpected decoded value {:?}", x),
                }
            }

            {
                let mut input: [Result<String, u32>; 2] = [Err(7), Err(42)];
                match encode_decode(ctx, &mut input) {
                    [Err(ref err1), Err(ref err2)] if *err1 == 7 && *err2 == 42 => {}
                    x => panic!("unexpected decoded value {:?}", x),
                }
            }

            {
                let mut input = [Ok("abc".to_string()), Err(42)];
                match encode_decode(ctx, &mut input) {
                    [Ok(ref ok1), Err(ref err2)] if *ok1 == "abc".to_string() && *err2 == 42 => {}
                    x => panic!("unexpected decoded value {:?}", x),
                }
            }
        }
    }

    struct Foo {
        byte: u8,
        bignum: u64,
        string: String,
    }

    fidl_struct! {
        name: Foo,
        members: [
            byte {
                ty: u8,
                offset_v1: 0,
            },
            bignum {
                ty: u64,
                offset_v1: 8,
            },
            string {
                ty: String,
                offset_v1: 16,
            },
        ],
        padding: [
            {
                ty: u64,
                offset: 0,
                mask: 0xffffffffffffff00,
            },
        ],
        size_v1: 32,
        align_v1: 8,
    }

    #[test]
    fn encode_decode_struct() {
        for ctx in CONTEXTS {
            let out_foo = encode_decode(
                ctx,
                &mut Some(Box::new(Foo { byte: 5, bignum: 22, string: "hello world".to_string() })),
            )
            .expect("should be some");

            assert_eq!(out_foo.byte, 5);
            assert_eq!(out_foo.bignum, 22);
            assert_eq!(out_foo.string, "hello world");

            let out_foo: Option<Box<Foo>> = encode_decode(ctx, &mut Box::new(None));
            assert!(out_foo.is_none());
        }
    }

    #[test]
    fn decode_struct_with_invalid_padding_fails() {
        for ctx in CONTEXTS {
            let foo = &mut Foo { byte: 0, bignum: 0, string: String::new() };
            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, foo).expect("Encoding failed");

            buf[1] = 42;
            let out = &mut Foo::new_empty();
            let result =
                Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), out);
            assert_matches!(
                result,
                Err(Error::NonZeroPadding { padding_start: 1, non_zero_pos: 1 })
            );
        }
    }

    #[test]
    fn encode_decode_tuple() {
        for ctx in CONTEXTS {
            let mut start: (&mut u8, &mut u64, &mut String) =
                (&mut 5, &mut 10, &mut "foo".to_string());
            let mut out: (u8, u64, String) = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut start)
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");

            assert_eq!(*start.0, out.0);
            assert_eq!(*start.1, out.1);
            assert_eq!(*start.2, out.2);
        }
    }

    #[test]
    fn encode_decode_struct_as_tuple() {
        for ctx in CONTEXTS {
            let mut start = Foo { byte: 5, bignum: 10, string: "foo".to_string() };
            let mut out: (u8, u64, String) = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut start)
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");

            assert_eq!(start.byte, out.0);
            assert_eq!(start.bignum, out.1);
            assert_eq!(start.string, out.2);
        }
    }

    #[test]
    fn encode_decode_tuple_as_struct() {
        for ctx in CONTEXTS {
            let mut start = (&mut 5u8, &mut 10u64, &mut "foo".to_string());
            let mut out: Foo = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut start)
                .expect("Encoding failed");
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");

            assert_eq!(*start.0, out.byte);
            assert_eq!(*start.1, out.bignum);
            assert_eq!(*start.2, out.string);
        }
    }

    #[test]
    fn encode_decode_tuple_msg() {
        for ctx in CONTEXTS {
            let mut body_start = (&mut "foo".to_string(), &mut 5);
            let mut body_out: (String, u8) = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut body_start).unwrap();
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut body_out)
                .unwrap();

            assert_eq!(body_start.0, &mut body_out.0);
            assert_eq!(body_start.1, &mut body_out.1);
        }
    }

    pub struct MyTable {
        pub num: Option<i32>,
        pub num_none: Option<i32>,
        pub string: Option<String>,
        pub handle: Option<Handle>,
        pub unknown_data: Option<BTreeMap<u64, UnknownData>>,
        #[deprecated = "Do not use MyTable"]
        pub __non_exhaustive: (),
    }

    fidl_table! {
        name: MyTable,
        members: [
            num {
                ty: i32,
                ordinal: 1,
            },
            num_none {
                ty: i32,
                ordinal: 2,
            },
            string {
                ty: String,
                ordinal: 3,
            },
            handle {
                ty: Handle,
                ordinal: 4,
                handle_metadata: {
                    handle_subtype: ObjectType::NONE,
                    handle_rights: Rights::SAME_RIGHTS,
                },
            },
        ],
        resource_unknown_member: unknown_data,
    }

    struct EmptyTableCompiles {
        pub unknown_data: Option<BTreeMap<u64, Vec<u8>>>,
        #[deprecated = "Do not use __non_exhaustive"]
        pub __non_exhaustive: (),
    }
    fidl_table! {
        name: EmptyTableCompiles,
        members: [],
        value_unknown_member: unknown_data,
    }

    struct TablePrefix {
        num: Option<i32>,
        num_none: Option<i32>,
        pub unknown_data: Option<BTreeMap<u64, Vec<u8>>>,
        #[deprecated = "Do not use __non_exhaustive"]
        pub __non_exhaustive: (),
    }

    fidl_table! {
        name: TablePrefix,
        members: [
            num {
                ty: i32,
                ordinal: 1,
            },
            num_none {
                ty: i32,
                ordinal: 2,
            },
        ],
        value_unknown_member: unknown_data,
    }

    #[test]
    fn empty_table() {
        let mut table: MyTable = MyTable::EMPTY;
        assert_eq!(None, table.num);
        table = MyTable { num: Some(32), ..MyTable::EMPTY };
        assert_eq!(Some(32), table.num);
        assert_eq!(None, table.string);
    }

    #[test]
    fn table_encode_prefix_decode_full() {
        for ctx in CONTEXTS {
            let mut table_prefix_in = TablePrefix { num: Some(5), ..TablePrefix::EMPTY };
            let mut table_out: MyTable = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut table_prefix_in).unwrap();
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut table_out)
                .unwrap();

            assert_eq!(table_out.num, Some(5));
            assert_eq!(table_out.num_none, None);
            assert_eq!(table_out.string, None);
            assert_eq!(table_out.handle, None);
        }
    }

    #[test]
    fn table_encode_omits_none_tail() {
        for ctx in CONTEXTS {
            // "None" fields at the tail of a table shouldn't be encoded at all.
            let mut table_in = MyTable {
                num: Some(5),
                // The rest of the fields should all be omitted in the encoded repr,
                // allowing decoding of the prefix to succeed.
                num_none: None,
                string: None,
                handle: None,
                ..MyTable::EMPTY
            };
            let mut table_prefix_out: TablePrefix = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut table_in).unwrap();
            Decoder::decode_with_context(
                ctx,
                buf,
                &mut to_handle_info(handle_buf),
                &mut table_prefix_out,
            )
            .unwrap();

            assert_eq!(table_prefix_out.num, Some(5));
            assert_eq!(table_prefix_out.num_none, None);
        }
    }

    #[test]
    fn table_decode_ignores_unrecognized_tail() {
        for ctx in CONTEXTS {
            let mut table_in = MyTable {
                num: Some(5),
                num_none: None,
                string: Some("foo".to_string()),
                handle: None,
                ..MyTable::EMPTY
            };
            let mut table_prefix_out: TablePrefix = Decodable::new_empty();

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut table_in).unwrap();
            Decoder::decode_with_context(
                ctx,
                buf,
                &mut to_handle_info(handle_buf),
                &mut table_prefix_out,
            )
            .unwrap();
            assert_eq!(table_prefix_out.num, Some(5));
            assert_eq!(table_prefix_out.num_none, None);
        }
    }

    #[derive(Debug, PartialEq)]
    pub struct SimpleTable {
        x: Option<i64>,
        y: Option<i64>,
        unknown_data: Option<BTreeMap<u64, Vec<u8>>>,
        #[deprecated = "Do not use __non_exhaustive"]
        pub __non_exhaustive: (),
    }

    fidl_table! {
        name: SimpleTable,
        members: [
            x {
                ty: i64,
                ordinal: 1,
            },
            y {
                ty: i64,
                ordinal: 5,
            },
        ],
        value_unknown_member: unknown_data,
    }

    #[derive(Debug, PartialEq)]
    pub struct TableWithStringAndVector {
        foo: Option<String>,
        bar: Option<i32>,
        baz: Option<Vec<u8>>,
        unknown_data: Option<BTreeMap<u64, Vec<u8>>>,
        #[deprecated = "Do not use __non_exhaustive"]
        pub __non_exhaustive: (),
    }

    fidl_table! {
        name: TableWithStringAndVector,
        members: [
            foo {
                ty: String,
                ordinal: 1,
            },
            bar {
                ty: i32,
                ordinal: 2,
            },
            baz {
                ty: Vec<u8>,
                ordinal: 3,
            },
        ],
        value_unknown_member: unknown_data,
    }

    #[test]
    fn table_golden_simple_table_with_xy() {
        let simple_table_with_xy: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, // max ordinal
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
            8, 0, 0, 0, 0, 0, 0, 0, // envelope 1: num bytes / num handles
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 2: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 3: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 4: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            8, 0, 0, 0, 0, 0, 0, 0, // envelope 5: num bytes / num handles
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
            42, 0, 0, 0, 0, 0, 0, 0, // field X
            67, 0, 0, 0, 0, 0, 0, 0, // field Y
        ];
        for ctx in CONTEXTS {
            encode_assert_bytes(
                ctx,
                SimpleTable { x: Some(42), y: Some(67), ..SimpleTable::EMPTY },
                simple_table_with_xy,
            );
        }
    }

    #[test]
    fn table_golden_simple_table_with_y() {
        let simple_table_with_y: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, // max ordinal
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 1: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 2: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 3: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            0, 0, 0, 0, 0, 0, 0, 0, // envelope 4: num bytes / num handles
            0, 0, 0, 0, 0, 0, 0, 0, // no alloc
            8, 0, 0, 0, 0, 0, 0, 0, // envelope 5: num bytes / num handles
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
            67, 0, 0, 0, 0, 0, 0, 0, // field Y
        ];
        for ctx in CONTEXTS {
            encode_assert_bytes(
                ctx,
                SimpleTable { x: None, y: Some(67), ..SimpleTable::EMPTY },
                simple_table_with_y,
            );
        }
    }

    #[test]
    fn table_golden_string_and_vector_hello_27() {
        let table_with_string_and_vector_hello_27: &[u8] = &[
            2, 0, 0, 0, 0, 0, 0, 0, // max ordinal
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
            24, 0, 0, 0, 0, 0, 0, 0, // envelope 1: num bytes / num handles
            255, 255, 255, 255, 255, 255, 255, 255, // envelope 1: alloc present
            8, 0, 0, 0, 0, 0, 0, 0, // envelope 2: num bytes / num handles
            255, 255, 255, 255, 255, 255, 255, 255, // envelope 2: alloc present
            5, 0, 0, 0, 0, 0, 0, 0, // element 1: length
            255, 255, 255, 255, 255, 255, 255, 255, // element 1: alloc present
            104, 101, 108, 108, 111, 0, 0, 0, // element 1: hello
            27, 0, 0, 0, 0, 0, 0, 0, // element 2: value
        ];
        for ctx in CONTEXTS {
            encode_assert_bytes(
                ctx,
                TableWithStringAndVector {
                    foo: Some("hello".to_string()),
                    bar: Some(27),
                    baz: None,
                    ..TableWithStringAndVector::EMPTY
                },
                table_with_string_and_vector_hello_27,
            );
        }
    }

    #[test]
    fn table_golden_empty_table() {
        let empty_table: &[u8] = &[
            0, 0, 0, 0, 0, 0, 0, 0, // max ordinal
            255, 255, 255, 255, 255, 255, 255, 255, // alloc present
        ];

        for ctx in CONTEXTS {
            encode_assert_bytes(ctx, SimpleTable::EMPTY, empty_table);
        }
    }

    #[derive(Debug)]
    pub struct TableWithGaps {
        second: Option<i32>,
        fourth: Option<i32>,
        unknown_data: Option<BTreeMap<u64, Vec<u8>>>,
        #[deprecated = "Do not use __non_exhaustive"]
        pub __non_exhaustive: (),
    }

    fidl_table! {
        name: TableWithGaps,
        members: [
            second {
                ty: i32,
                ordinal: 2,
            },
            fourth {
                ty: i32,
                ordinal: 4,
            },
        ],
        value_unknown_member: unknown_data,
    }

    #[test]
    fn encode_decode_table_with_gaps() {
        for ctx in CONTEXTS {
            let mut table =
                TableWithGaps { second: Some(1), fourth: Some(2), ..TableWithGaps::EMPTY };
            let table_out = encode_decode(ctx, &mut table);
            assert_eq!(table_out.second, Some(1));
            assert_eq!(table_out.fourth, Some(2));
        }
    }

    #[test]
    fn encode_empty_envelopes_for_reserved_table_fields() {
        for ctx in CONTEXTS {
            let mut table =
                TableWithGaps { second: Some(1), fourth: Some(2), ..TableWithGaps::EMPTY };
            let buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, &mut Vec::new(), &mut table).unwrap();

            // Expected layout:
            //     0x00 table header
            //     0x10 envelope 1 (reserved)
            //     0x20 envelope 2 (second)
            //     0x30 envelope 3 (reserved)
            //     0x40 envelope 4 (fourth)
            assert_eq!(&buf[0x10..0x20], &[0; 16]);
            assert_eq!(&buf[0x30..0x40], &[0; 16]);
        }
    }

    #[test]
    fn decode_table_missing_gaps() {
        struct TableWithoutGaps {
            first: Option<i32>,
            second: Option<i32>,
            pub unknown_data: Option<BTreeMap<u64, Vec<u8>>>,
            #[deprecated = "Do not use __non_exhaustive"]
            pub __non_exhaustive: (),
        }
        fidl_table! {
            name: TableWithoutGaps,
            members: [
                first {
                    ty: i32,
                    ordinal: 1,
                },
                second {
                    ty: i32,
                    ordinal: 2,
                },
            ],
            value_unknown_member: unknown_data,
        }

        for ctx in CONTEXTS {
            // This test shows what would happen when decoding a TableWithGaps
            // that was incorrectly encoded _without_ gaps.
            //
            //     Ordinal:  #1     #2      #3     #4
            //     Encoded:  first second
            //     Decoding: _____ second  _____ fourth
            //
            // Field #1 is assumed to be a new field in a reserved slot (i.e.
            // the sender is newer than us), so it is ignored. Fields #3 and #4
            // are assumed to be None because the tail is omitted.
            let mut table =
                TableWithoutGaps { first: Some(1), second: Some(2), ..TableWithoutGaps::EMPTY };
            let buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, &mut Vec::new(), &mut table).unwrap();

            let mut out = TableWithGaps::new_empty();
            Decoder::decode_with_context(ctx, buf, &mut Vec::new(), &mut out).unwrap();
            assert_eq!(out.second, Some(2));
            assert_eq!(out.fourth, None);
        }
    }

    #[derive(Debug, PartialEq, zerocopy::AsBytes, zerocopy::FromBytes)]
    #[repr(C)]
    pub struct DirectCopyStruct {
        a: u64,
        b: u32,
        c: u16,
        d: u16,
    }
    fidl_struct_copy! {
        name: DirectCopyStruct,
        members: [
            a {
                ty: u64,
                offset_v1: 0,
            },
            b {
                ty: u32,
                offset_v1: 8,
            },
            c {
                ty: u16,
                offset_v1: 12,
            },
            d {
                ty: u16,
                offset_v1: 14,
            },
        ],
        padding: [],
        size_v1: 16,
        align_v1: 8,
    }

    #[test]
    fn direct_copy_struct_encode() {
        let bytes = &[
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, //
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, //
        ];
        let mut obj =
            DirectCopyStruct { a: 0x0807060504030201, b: 0x0c0b0a09, c: 0x0e0d, d: 0x100f };

        for ctx in CONTEXTS {
            encode_assert_bytes(ctx, &mut obj, bytes);
        }
    }

    #[test]
    fn direct_copy_struct_decode() {
        let bytes = &[
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, //
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, //
        ];
        let obj = DirectCopyStruct { a: 0x0807060504030201, b: 0x0c0b0a09, c: 0x0e0d, d: 0x100f };

        for ctx in CONTEXTS {
            let mut out = DirectCopyStruct::new_empty();
            Decoder::decode_with_context(ctx, bytes, &mut [], &mut out).expect("Decoding failed");
            assert_eq!(out, obj);
        }
    }

    #[derive(Debug, PartialEq)]
    pub struct Int64Struct {
        x: u64,
    }
    fidl_struct! {
        name: Int64Struct,
        members: [
            x {
                ty: u64,
                offset_v1: 0,
            },
        ],
        padding: [],
        size_v1: 8,
        align_v1: 8,
    }

    // This is a resource union, as a resource member is added in
    // TestSampleXUnionExpanded
    #[derive(Debug, PartialEq)]
    pub enum TestSampleXUnion {
        U(u32),
        St(SimpleTable),
        __Unknown { ordinal: u64, data: UnknownData },
    }
    fidl_union! {
        name: TestSampleXUnion,
        members: [
            U {
                ty: u32,
                ordinal: 0x29df47a5,
            },
            St {
                ty: SimpleTable,
                ordinal: 0x6f317664,
            },
        ],
        resource_unknown_member: __Unknown,
    }

    #[derive(Debug, PartialEq)]
    pub enum TestSampleXUnionExpanded {
        SomethinElse(Handle),
        __Unknown { ordinal: u64, data: UnknownData },
    }
    fidl_union! {
        name: TestSampleXUnionExpanded,
        members: [
            SomethinElse {
                ty: Handle,
                ordinal: 55,
                handle_metadata: {
                    handle_subtype: ObjectType::NONE,
                    handle_rights: Rights::SAME_RIGHTS,
                },
            },
        ],
        resource_unknown_member: __Unknown,
    }

    #[test]
    fn encode_decode_transaction_msg() {
        for ctx in CONTEXTS {
            let header = TransactionHeader { tx_id: 4, ordinal: 6, flags: [0; 3], magic_number: 1 };
            let body = "hello".to_string();

            let start = &mut TransactionMessage { header, body: &mut body.clone() };

            let (buf, handles) = (&mut vec![], &mut vec![]);
            Encoder::encode_with_context(ctx, buf, handles, start).expect("Encoding failed");

            let (out_header, out_buf) =
                decode_transaction_header(&**buf).expect("Decoding header failed");
            assert_eq!(header, out_header);

            let mut body_out = String::new();
            Decoder::decode_into(&header, out_buf, &mut to_handle_info(handles), &mut body_out)
                .expect("Decoding body failed");
            assert_eq!(body, body_out);
        }
    }

    #[test]
    fn direct_encode_transaction_header() {
        let bytes = &[
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, //
            0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        ];
        let mut header = TransactionHeader { tx_id: 4, ordinal: 6, flags: [0; 3], magic_number: 1 };

        for ctx in CONTEXTS {
            encode_assert_bytes(ctx, &mut header, bytes);
        }
    }

    #[test]
    fn direct_decode_transaction_header() {
        let bytes = &[
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, //
            0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        ];
        let header = TransactionHeader { tx_id: 4, ordinal: 6, flags: [0; 3], magic_number: 1 };

        for ctx in CONTEXTS {
            let mut out = TransactionHeader::new_empty();
            Decoder::decode_with_context(ctx, bytes, &mut [], &mut out).expect("Decoding failed");
            assert_eq!(out, header);
        }
    }

    #[test]
    fn encode_decode_persistent_combined() {
        let mut body = "hello".to_string();

        let buf = encode_persistent(&mut body).expect("Encoding failed");
        let body_out = decode_persistent::<String>(&buf).expect("Decoding failed");

        assert_eq!(body, body_out);
    }

    #[test]
    fn encode_decode_persistent_separate() {
        let mut body = "hello".to_string();
        let mut another_body = "world".to_string();

        let mut header = create_persistent_header();
        let buf_header = encode_persistent_header(&mut header).expect("Header encoding failed");
        let buf_body = encode_persistent_body(&mut body, &header).expect("Body encoding failed");
        let buf_another_body =
            encode_persistent_body(&mut another_body, &header).expect("Body encoding failed");

        let header_out = decode_persistent_header(&buf_header).expect("Header decoding failed");
        assert_eq!(header, header_out);
        let body_out =
            decode_persistent_body::<String>(&buf_body, &header).expect("Body decoding failed");
        assert_eq!(body, body_out);
        let another_body_out = decode_persistent_body::<String>(&buf_another_body, &header)
            .expect("Another body decoding failed");
        assert_eq!(another_body, another_body_out);
    }

    #[test]
    fn array_of_arrays() {
        for ctx in CONTEXTS {
            let mut input = &mut [&mut [1u32, 2, 3, 4, 5], &mut [5, 4, 3, 2, 1]];
            let (bytes, handles) = (&mut vec![], &mut vec![]);
            assert!(Encoder::encode_with_context(ctx, bytes, handles, &mut input).is_ok());

            let mut output = <[[u32; 5]; 2]>::new_empty();
            Decoder::decode_with_context(ctx, bytes, &mut to_handle_info(handles), &mut output)
                .expect(
                    format!(
                        "Array decoding failed\n\
                     bytes: {:X?}",
                        bytes
                    )
                    .as_str(),
                );

            assert_eq!(
                input,
                output.iter_mut().map(|v| v.as_mut()).collect::<Vec<_>>().as_mut_slice()
            );
        }
    }

    #[test]
    fn xunion_with_64_bit_ordinal() {
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        enum BigOrdinal {
            X(u64),
        }
        fidl_union! {
            name: BigOrdinal,
            members: [
                X {
                    ty: u64,
                    ordinal: 0xffffffffffffffffu64,
                },
            ],
        };

        for ctx in CONTEXTS {
            let mut x = BigOrdinal::X(0);
            assert_eq!(x.ordinal(), u64::MAX);
            assert_eq!(encode_decode(ctx, &mut x).ordinal(), u64::MAX);
        }
    }

    #[test]
    fn extra_data_is_disallowed() {
        for ctx in CONTEXTS {
            let mut output = ();
            assert_matches!(
                Decoder::decode_with_context(ctx, &[0], &mut [], &mut output),
                Err(Error::ExtraBytes)
            );
            assert_matches!(
                Decoder::decode_with_context(
                    ctx,
                    &[],
                    &mut [HandleInfo {
                        handle: Handle::invalid(),
                        object_type: ObjectType::NONE,
                        rights: Rights::NONE,
                    }],
                    &mut output
                ),
                Err(Error::ExtraHandles)
            );
        }
    }

    #[test]
    fn encode_default_context() {
        let buf = &mut Vec::new();
        Encoder::encode(buf, &mut Vec::new(), &mut 1u8).expect("Encoding failed");
        assert_eq!(&**buf, &[1u8, 0, 0, 0, 0, 0, 0, 0]);
    }
}

#[cfg(target_os = "fuchsia")]
#[cfg(test)]
mod zx_test {
    use super::test::*;
    use super::*;
    use crate::handle::AsHandleRef;
    use fuchsia_zircon as zx;

    fn to_handle_info(handles: &mut Vec<HandleDisposition<'static>>) -> Vec<HandleInfo> {
        handles
            .drain(..)
            .map(|h| {
                assert_eq!(h.result, Status::OK);
                if let HandleOp::Move(mut handle) = h.handle_op {
                    return HandleInfo {
                        handle: mem::replace(&mut handle, Handle::invalid()),
                        object_type: h.object_type,
                        rights: h.rights,
                    };
                }
                panic!("expected HandleOp::Move");
            })
            .collect()
    }

    #[test]
    fn encode_handle() {
        for ctx in CONTEXTS {
            let mut handle = Handle::from(zx::Port::create().expect("Port creation failed"));
            let raw_handle = handle.raw_handle();

            wrap_handle_metadata!(HandleWrapper, ObjectType::NONE, Rights::SAME_RIGHTS);

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut HandleWrapper(&mut handle))
                .expect("Encoding failed");

            assert!(handle.is_invalid());

            let mut handle_out = HandleWrapper(Handle::new_empty());
            Decoder::decode_with_context(
                ctx,
                buf,
                &mut to_handle_info(handle_buf),
                &mut handle_out,
            )
            .expect("Decoding failed");

            assert_eq!(raw_handle, handle_out.into_inner().raw_handle());
        }
    }

    #[test]
    fn encode_decode_table() {
        for ctx in CONTEXTS {
            // create a random handle to encode and then decode.
            let handle = zx::Vmo::create(1024).expect("vmo creation failed");
            let raw_handle = handle.raw_handle();
            let mut starting_table = MyTable {
                num: Some(5),
                string: Some("foo".to_string()),
                handle: Some(handle.into_handle()),
                ..MyTable::EMPTY
            };
            let table_out = encode_decode(ctx, &mut starting_table);
            assert_eq!(table_out.num, Some(5));
            assert_eq!(table_out.num_none, None);
            assert_eq!(table_out.string, Some("foo".to_string()));
            assert_eq!(table_out.handle.unwrap().raw_handle(), raw_handle);
        }
    }

    #[test]
    fn flexible_xunion_unknown_variant_transparent_passthrough() {
        for ctx in CONTEXTS {
            let handle = Handle::from(zx::Port::create().expect("Port creation failed"));
            let raw_handle = handle.raw_handle();

            let mut input = TestSampleXUnionExpanded::SomethinElse(handle);
            // encode expanded and decode as xunion w/ missing variant
            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut input)
                .expect("Encoding TestSampleXUnionExpanded failed");

            let mut intermediate_missing_variant = TestSampleXUnion::new_empty();
            Decoder::decode_with_context(
                ctx,
                buf,
                &mut to_handle_info(handle_buf),
                &mut intermediate_missing_variant,
            )
            .expect("Decoding TestSampleXUnion failed");

            // Ensure we've recorded the unknown variant
            #[allow(deprecated)]
            if !matches!(intermediate_missing_variant, TestSampleXUnion::__Unknown { .. }) {
                panic!("unexpected variant")
            }

            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(ctx, buf, handle_buf, &mut intermediate_missing_variant)
                .expect("encoding unknown variant failed");

            let mut out = TestSampleXUnionExpanded::new_empty();
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding final output failed");

            if let TestSampleXUnionExpanded::SomethinElse(handle_out) = out {
                assert_eq!(raw_handle, handle_out.raw_handle());
            } else {
                panic!("wrong final variant")
            }
        }
    }

    #[test]
    fn encode_epitaph() {
        for ctx in CONTEXTS {
            let buf = &mut Vec::new();
            let handle_buf = &mut Vec::new();
            Encoder::encode_with_context(
                ctx,
                buf,
                handle_buf,
                &mut EpitaphBody { error: zx::Status::UNAVAILABLE },
            )
            .expect("encoding failed");
            assert_eq!(&**buf, &[0xe4, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00]);
            let mut out: EpitaphBody = EpitaphBody { error: zx::Status::OK };
            Decoder::decode_with_context(ctx, buf, &mut to_handle_info(handle_buf), &mut out)
                .expect("Decoding failed");
            assert_eq!(EpitaphBody { error: zx::Status::UNAVAILABLE }, out);
        }
    }
}
