use crate::component::func::{Func, Memory, MemoryMut, Options};
use crate::store::StoreOpaque;
use crate::{AsContext, AsContextMut, StoreContext, StoreContextMut, ValRaw};
use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::fmt;
use std::marker;
use std::mem::{self, MaybeUninit};
use std::str;
use wasmtime_environ::component::{
    ComponentTypes, InterfaceType, StringEncoding, MAX_FLAT_PARAMS, MAX_FLAT_RESULTS,
};

/// A statically-typed version of [`Func`] which takes `Params` as input and
/// returns `Return`.
///
/// This is an efficient way to invoke a WebAssembly component where if the
/// inputs and output are statically known this can eschew the vast majority of
/// machinery and checks when calling WebAssembly. This is the most optimized
/// way to call a WebAssembly component.
///
/// Note that like [`Func`] this is a pointer within a [`Store`](crate::Store)
/// and usage will panic if used with the wrong store.
///
/// This type is primarily created with the [`Func::typed`] API.
pub struct TypedFunc<Params, Return> {
    func: Func,

    // The definition of this field is somewhat subtle and may be surprising.
    // Naively one might expect something like
    //
    //      _marker: marker::PhantomData<fn(Params) -> Return>,
    //
    // Since this is a function pointer after all. The problem with this
    // definition though is that it imposes the wrong variance on `Params` from
    // what we want. Abstractly a `fn(Params)` is able to store `Params` within
    // it meaning you can only give it `Params` that live longer than the
    // function pointer.
    //
    // With a component model function, however, we're always copying data from
    // the host into the guest, so we are never storing pointers to `Params`
    // into the guest outside the duration of a `call`, meaning we can actually
    // accept values in `TypedFunc::call` which live for a shorter duration
    // than the `Params` argument on the struct.
    //
    // This all means that we don't use a phantom function pointer, but instead
    // feign phantom storage here to get the variance desired.
    _marker: marker::PhantomData<(Params, Return)>,
}

impl<Params, Return> Copy for TypedFunc<Params, Return> {}

impl<Params, Return> Clone for TypedFunc<Params, Return> {
    fn clone(&self) -> TypedFunc<Params, Return> {
        *self
    }
}

impl<Params, Return> TypedFunc<Params, Return>
where
    Params: ComponentParams + Lower,
    Return: Lift,
{
    /// Creates a new [`TypedFunc`] from the provided component [`Func`],
    /// unsafely asserting that the underlying function takes `Params` as
    /// input and returns `Return`.
    ///
    /// # Unsafety
    ///
    /// This is an unsafe function because it does not verify that the [`Func`]
    /// provided actually implements this signature. It's up to the caller to
    /// have performed some other sort of check to ensure that the signature is
    /// correct.
    pub unsafe fn new_unchecked(func: Func) -> TypedFunc<Params, Return> {
        TypedFunc {
            _marker: marker::PhantomData,
            func,
        }
    }

    /// Returns the underlying un-typed [`Func`] that this [`TypedFunc`]
    /// references.
    pub fn func(&self) -> &Func {
        &self.func
    }

    /// Calls the underlying WebAssembly component function using the provided
    /// `params` as input.
    ///
    /// This method is used to enter into a component. Execution happens within
    /// the `store` provided. The `params` are copied into WebAssembly memory
    /// as appropriate and a core wasm function is invoked.
    ///
    /// # Post-return
    ///
    /// In the component model each function can have a "post return" specified
    /// which allows cleaning up the arguments returned to the host. For example
    /// if WebAssembly returns a string to the host then it might be a uniquely
    /// allocated string which, after the host finishes processing it, needs to
    /// be deallocated in the wasm instance's own linear memory to prevent
    /// memory leaks in wasm itself. The `post-return` canonical abi option is
    /// used to configured this.
    ///
    /// To accommodate this feature of the component model after invoking a
    /// function via [`TypedFunc::call`] you must next invoke
    /// [`TypedFunc::post_return`]. Note that the return value of the function
    /// should be processed between these two function calls. The return value
    /// continues to be usable from an embedder's perspective after
    /// `post_return` is called, but after `post_return` is invoked it may no
    /// longer retain the same value that the wasm module originally returned.
    ///
    /// Also note that [`TypedFunc::post_return`] must be invoked irrespective
    /// of whether the canonical ABI option `post-return` was configured or not.
    /// This means that embedders must unconditionally call
    /// [`TypedFunc::post_return`] when a function returns. If this function
    /// call returns an error, however, then [`TypedFunc::post_return`] is not
    /// required.
    ///
    /// # Errors
    ///
    /// This function can return an error for a number of reasons:
    ///
    /// * If the wasm itself traps during execution.
    /// * If the wasm traps while copying arguments into memory.
    /// * If the wasm provides bad allocation pointers when copying arguments
    ///   into memory.
    /// * If the wasm returns a value which violates the canonical ABI.
    /// * If this function's instances cannot be entered, for example if the
    ///   instance is currently calling a host function.
    /// * If a previous function call occurred and the corresponding
    ///   `post_return` hasn't been invoked yet.
    ///
    /// In general there are many ways that things could go wrong when copying
    /// types in and out of a wasm module with the canonical ABI, and certain
    /// error conditions are specific to certain types. For example a
    /// WebAssembly module can't return an invalid `char`. When allocating space
    /// for this host to copy a string into the returned pointer must be
    /// in-bounds in memory.
    ///
    /// If an error happens then the error should contain detailed enough
    /// information to understand which part of the canonical ABI went wrong
    /// and what to inspect.
    ///
    /// # Panics
    ///
    /// This function will panic if `store` does not own this function.
    pub fn call(&self, mut store: impl AsContextMut, params: Params) -> Result<Return> {
        let store = &mut store.as_context_mut();
        // Note that this is in theory simpler than it might read at this time.
        // Here we're doing a runtime dispatch on the `flatten_count` for the
        // params/results to see whether they're inbounds. This creates 4 cases
        // to handle. In reality this is a highly optimizable branch where LLVM
        // will easily figure out that only one branch here is taken.
        //
        // Otherwise this current construction is done to ensure that the stack
        // space reserved for the params/results is always of the appropriate
        // size (as the params/results needed differ depending on the "flatten"
        // count)
        if Params::flatten_count() <= MAX_FLAT_PARAMS {
            if Return::flatten_count() <= MAX_FLAT_RESULTS {
                self.func.call_raw(
                    store,
                    &params,
                    Self::lower_stack_args,
                    Self::lift_stack_result,
                )
            } else {
                self.func.call_raw(
                    store,
                    &params,
                    Self::lower_stack_args,
                    Self::lift_heap_result,
                )
            }
        } else {
            if Return::flatten_count() <= MAX_FLAT_RESULTS {
                self.func.call_raw(
                    store,
                    &params,
                    Self::lower_heap_args,
                    Self::lift_stack_result,
                )
            } else {
                self.func.call_raw(
                    store,
                    &params,
                    Self::lower_heap_args,
                    Self::lift_heap_result,
                )
            }
        }
    }

    /// Lower parameters directly onto the stack specified by the `dst`
    /// location.
    ///
    /// This is only valid to call when the "flatten count" is small enough, or
    /// when the canonical ABI says arguments go through the stack rather than
    /// the heap.
    fn lower_stack_args<T>(
        store: &mut StoreContextMut<'_, T>,
        options: &Options,
        params: &Params,
        dst: &mut MaybeUninit<Params::Lower>,
    ) -> Result<()> {
        assert!(Params::flatten_count() <= MAX_FLAT_PARAMS);
        params.lower(store, options, dst)?;
        Ok(())
    }

    /// Lower parameters onto a heap-allocated location.
    ///
    /// This is used when the stack space to be used for the arguments is above
    /// the `MAX_FLAT_PARAMS` threshold. Here the wasm's `realloc` function is
    /// invoked to allocate space and then parameters are stored at that heap
    /// pointer location.
    fn lower_heap_args<T>(
        store: &mut StoreContextMut<'_, T>,
        options: &Options,
        params: &Params,
        dst: &mut MaybeUninit<ValRaw>,
    ) -> Result<()> {
        assert!(Params::flatten_count() > MAX_FLAT_PARAMS);

        // Memory must exist via validation if the arguments are stored on the
        // heap, so we can create a `MemoryMut` at this point. Afterwards
        // `realloc` is used to allocate space for all the arguments and then
        // they're all stored in linear memory.
        //
        // Note that `realloc` will bake in a check that the returned pointer is
        // in-bounds.
        let mut memory = MemoryMut::new(store.as_context_mut(), options);
        let ptr = memory.realloc(0, 0, Params::ALIGN32, Params::SIZE32)?;
        params.store(&mut memory, ptr)?;

        // Note that the pointer here is stored as a 64-bit integer. This allows
        // this to work with either 32 or 64-bit memories. For a 32-bit memory
        // it'll just ignore the upper 32 zero bits, and for 64-bit memories
        // this'll have the full 64-bits. Note that for 32-bit memories the call
        // to `realloc` above guarantees that the `ptr` is in-bounds meaning
        // that we will know that the zero-extended upper bits of `ptr` are
        // guaranteed to be zero.
        //
        // This comment about 64-bit integers is also referred to below with
        // "WRITEPTR64".
        dst.write(ValRaw::i64(ptr as i64));

        Ok(())
    }

    /// Lift the result of a function directly from the stack result.
    ///
    /// This is only used when the result fits in the maximum number of stack
    /// slots.
    fn lift_stack_result(
        store: &StoreOpaque,
        options: &Options,
        dst: &Return::Lower,
    ) -> Result<Return> {
        assert!(Return::flatten_count() <= MAX_FLAT_RESULTS);
        Return::lift(store, options, dst)
    }

    /// Lift the result of a function where the result is stored indirectly on
    /// the heap.
    fn lift_heap_result(store: &StoreOpaque, options: &Options, dst: &ValRaw) -> Result<Return> {
        assert!(Return::flatten_count() > MAX_FLAT_RESULTS);
        // FIXME: needs to read an i64 for memory64
        let ptr = usize::try_from(dst.get_u32())?;
        if ptr % usize::try_from(Return::ALIGN32)? != 0 {
            bail!("return pointer not aligned");
        }

        let memory = Memory::new(store, options);
        let bytes = memory
            .as_slice()
            .get(ptr..)
            .and_then(|b| b.get(..Return::SIZE32))
            .ok_or_else(|| anyhow::anyhow!("pointer out of bounds of memory"))?;
        Return::load(&memory, bytes)
    }

    /// See [`Func::post_return`]
    pub fn post_return(&self, store: impl AsContextMut) -> Result<()> {
        self.func.post_return(store)
    }
}

/// A trait representing a static list of parameters that can be passed to a
/// [`TypedFunc`].
///
/// This trait is implemented for a number of tuple types and is not expected
/// to be implemented externally. The contents of this trait are hidden as it's
/// intended to be an implementation detail of Wasmtime. The contents of this
/// trait are not covered by Wasmtime's stability guarantees.
///
/// For more information about this trait see [`Func::typed`] and
/// [`TypedFunc`].
//
// Note that this is an `unsafe` trait, and the unsafety means that
// implementations of this trait must be correct or otherwise [`TypedFunc`]
// would not be memory safe. The main reason this is `unsafe` is the
// `typecheck` function which must operate correctly relative to the `AsTuple`
// interpretation of the implementor.
pub unsafe trait ComponentParams: ComponentType {
    /// Performs a typecheck to ensure that this `ComponentParams` implementor
    /// matches the types of the types in `params`.
    #[doc(hidden)]
    fn typecheck_params(
        params: &[(Option<String>, InterfaceType)],
        types: &ComponentTypes,
    ) -> Result<()>;
}

/// A trait representing types which can be passed to and read from components
/// with the canonical ABI.
///
/// This trait is implemented for Rust types which can be communicated to
/// components. This is implemented for Rust types which correspond to
/// interface types in the component model of WebAssembly. The [`Func::typed`]
/// and [`TypedFunc`] Rust items are the main consumers of this trait.
///
/// For more information on this trait see the examples in [`Func::typed`].
///
/// The contents of this trait are hidden as it's intended to be an
/// implementation detail of Wasmtime. The contents of this trait are not
/// covered by Wasmtime's stability guarantees.
//
// Note that this is an `unsafe` trait as `TypedFunc`'s safety heavily relies on
// the correctness of the implementations of this trait. Some ways in which this
// trait must be correct to be safe are:
//
// * The `Lower` associated type must be a `ValRaw` sequence. It doesn't have to
//   literally be `[ValRaw; N]` but when laid out in memory it must be adjacent
//   `ValRaw` values and have a multiple of the size of `ValRaw` and the same
//   alignment.
//
// * The `lower` function must initialize the bits within `Lower` that are going
//   to be read by the trampoline that's used to enter core wasm. A trampoline
//   is passed `*mut Lower` and will read the canonical abi arguments in
//   sequence, so all of the bits must be correctly initialized.
//
// * The `size` and `align` functions must be correct for this value stored in
//   the canonical ABI. The `Cursor<T>` iteration of these bytes rely on this
//   for correctness as they otherwise eschew bounds-checking.
//
// There are likely some other correctness issues which aren't documented as
// well, this isn't intended to be an exhaustive list. It suffices to say,
// though, that correctness bugs in this trait implementation are highly likely
// to lead to security bugs, which again leads to the `unsafe` in the trait.
//
// Also note that this trait specifically is not sealed because we have a proc
// macro that generates implementations of this trait for external types in a
// `#[derive]`-like fashion.
pub unsafe trait ComponentType {
    /// Representation of the "lowered" form of this component value.
    ///
    /// Lowerings lower into core wasm values which are represented by `ValRaw`.
    /// This `Lower` type must be a list of `ValRaw` as either a literal array
    /// or a struct where every field is a `ValRaw`. This must be `Copy` (as
    /// `ValRaw` is `Copy`) and support all byte patterns. This being correct is
    /// one reason why the trait is unsafe.
    #[doc(hidden)]
    type Lower: Copy;

    /// The size, in bytes, that this type has in the canonical ABI.
    #[doc(hidden)]
    const SIZE32: usize;

    /// The alignment, in bytes, that this type has in the canonical ABI.
    #[doc(hidden)]
    const ALIGN32: u32;

    /// Returns the number of core wasm abi values will be used to represent
    /// this type in its lowered form.
    ///
    /// This divides the size of `Self::Lower` by the size of `ValRaw`.
    #[doc(hidden)]
    fn flatten_count() -> usize {
        assert!(mem::size_of::<Self::Lower>() % mem::size_of::<ValRaw>() == 0);
        assert!(mem::align_of::<Self::Lower>() == mem::align_of::<ValRaw>());
        mem::size_of::<Self::Lower>() / mem::size_of::<ValRaw>()
    }

    // FIXME: need SIZE64 and ALIGN64 probably

    /// Performs a type-check to see whether this component value type matches
    /// the interface type `ty` provided.
    #[doc(hidden)]
    fn typecheck(ty: &InterfaceType, types: &ComponentTypes) -> Result<()>;
}

/// Host types which can be passed to WebAssembly components.
///
/// This trait is implemented for all types that can be passed to components
/// either as parameters of component exports or returns of component imports.
/// This trait represents the ability to convert from the native host
/// representation to the canonical ABI.
//
// TODO: #[derive(Lower)]
// TODO: more docs here
pub unsafe trait Lower: ComponentType {
    /// Performs the "lower" function in the canonical ABI.
    ///
    /// This method will lower the given value into wasm linear memory. The
    /// `store` and `func` are provided in case memory is needed (e.g. for
    /// strings/lists) so `realloc` can be called. The `dst` is the destination
    /// to store the lowered results.
    ///
    /// Note that `dst` is a pointer to uninitialized memory. It's expected
    /// that `dst` is fully initialized by the time this function returns, hence
    /// the `unsafe` on the trait implementation.
    ///
    /// This will only be called if `typecheck` passes for `Op::Lower`.
    #[doc(hidden)]
    fn lower<T>(
        &self,
        store: &mut StoreContextMut<T>,
        options: &Options,
        dst: &mut MaybeUninit<Self::Lower>,
    ) -> Result<()>;

    /// Performs the "store" operation in the canonical ABI.
    ///
    /// This function will store `self` into the linear memory described by
    /// `memory` at the `offset` provided.
    ///
    /// It is expected that `offset` is a valid offset in memory for
    /// `Self::SIZE32` bytes. At this time that's not an unsafe contract as it's
    /// always re-checked on all stores, but this is something that will need to
    /// be improved in the future to remove extra bounds checks. For now this
    /// function will panic if there's a bug and `offset` isn't valid within
    /// memory.
    ///
    /// This will only be called if `typecheck` passes for `Op::Lower`.
    #[doc(hidden)]
    fn store<T>(&self, memory: &mut MemoryMut<'_, T>, offset: usize) -> Result<()>;
}

/// Host types which can be created from the canonical ABI.
//
// TODO: #[derive(Lower)]
// TODO: more docs here
pub unsafe trait Lift: Sized + ComponentType {
    /// Performs the "lift" operation in the canonical ABI.
    ///
    /// This will read the core wasm values from `src` and use the memory
    /// specified by `func` and `store` optionally if necessary. An instance of
    /// `Self` is then created from the values, assuming validation succeeds.
    ///
    /// Note that this has a default implementation but if `typecheck` passes
    /// for `Op::Lift` this needs to be overridden.
    #[doc(hidden)]
    fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self>;

    /// Performs the "load" operation in the canonical ABI.
    ///
    /// This is given the linear-memory representation of `Self` in the `bytes`
    /// array provided which is guaranteed to be `Self::SIZE32` bytes large. All
    /// of memory is then also described with `Memory` for bounds-checks and
    /// such as necessary for strings/lists.
    ///
    /// Note that this has a default implementation but if `typecheck` passes
    /// for `Op::Lift` this needs to be overridden.
    #[doc(hidden)]
    fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self>;
}

// Macro to help generate "forwarding implementations" of `ComponentType` to
// another type, used for wrappers in Rust like `&T`, `Box<T>`, etc. Note that
// these wrappers only implement lowering because lifting native Rust types
// cannot be done.
macro_rules! forward_type_impls {
    ($(($($generics:tt)*) $a:ty => $b:ty,)*) => ($(
        unsafe impl <$($generics)*> ComponentType for $a {
            type Lower = <$b as ComponentType>::Lower;

            const SIZE32: usize = <$b as ComponentType>::SIZE32;
            const ALIGN32: u32 = <$b as ComponentType>::ALIGN32;

            #[inline]
            fn typecheck(ty: &InterfaceType, types: &ComponentTypes) -> Result<()> {
                <$b as ComponentType>::typecheck(ty, types)
            }
        }
    )*)
}

forward_type_impls! {
    (T: ComponentType + ?Sized) &'_ T => T,
    (T: ComponentType + ?Sized) Box<T> => T,
    (T: ComponentType + ?Sized) std::rc::Rc<T> => T,
    (T: ComponentType + ?Sized) std::sync::Arc<T> => T,
    () String => str,
    (T: ComponentType) Vec<T> => [T],
}

macro_rules! forward_lowers {
    ($(($($generics:tt)*) $a:ty => $b:ty,)*) => ($(
        unsafe impl <$($generics)*> Lower for $a {
            fn lower<U>(
                &self,
                store: &mut StoreContextMut<U>,
                options: &Options,
                dst: &mut MaybeUninit<Self::Lower>,
            ) -> Result<()> {
                <$b as Lower>::lower(self, store, options, dst)
            }

            fn store<U>(&self, memory: &mut MemoryMut<'_, U>, offset: usize) -> Result<()> {
                <$b as Lower>::store(self, memory, offset)
            }
        }
    )*)
}

forward_lowers! {
    (T: Lower + ?Sized) &'_ T => T,
    (T: Lower + ?Sized) Box<T> => T,
    (T: Lower + ?Sized) std::rc::Rc<T> => T,
    (T: Lower + ?Sized) std::sync::Arc<T> => T,
    () String => str,
    (T: Lower) Vec<T> => [T],
}

macro_rules! forward_string_lifts {
    ($($a:ty,)*) => ($(
        unsafe impl Lift for $a {
            fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self> {
                Ok(<WasmStr as Lift>::lift(store, options, src)?.to_str_from_store(store)?.into())
            }

            fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
                Ok(<WasmStr as Lift>::load(memory, bytes)?.to_str_from_store(&memory.store)?.into())
            }
        }
    )*)
}

forward_string_lifts! {
    Box<str>,
    std::rc::Rc<str>,
    std::sync::Arc<str>,
    String,
}

macro_rules! forward_list_lifts {
    ($($a:ty,)*) => ($(
        unsafe impl <T: Lift> Lift for $a {
            fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self> {
                let list = <WasmList::<T> as Lift>::lift(store, options, src)?;
                (0..list.len).map(|index| list.get_from_store(store, index).unwrap()).collect()
            }

            fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
                let list = <WasmList::<T> as Lift>::load(memory, bytes)?;
                (0..list.len).map(|index| list.get_from_store(&memory.store, index).unwrap()).collect()
            }
        }
    )*)
}

forward_list_lifts! {
    Box<[T]>,
    std::rc::Rc<[T]>,
    std::sync::Arc<[T]>,
    Vec<T>,
}

// Macro to help generate `ComponentType` implementations for primitive types
// such as integers, char, bool, etc.
macro_rules! integers {
    ($($primitive:ident = $ty:ident in $field:ident/$get:ident,)*) => ($(
        unsafe impl ComponentType for $primitive {
            type Lower = ValRaw;

            const SIZE32: usize = mem::size_of::<$primitive>();

            // Note that this specifically doesn't use `align_of` as some
            // host platforms have a 4-byte alignment for primitive types but
            // the canonical abi always has the same size/alignment for these
            // types.
            const ALIGN32: u32 = mem::size_of::<$primitive>() as u32;

            fn typecheck(ty: &InterfaceType, _types: &ComponentTypes) -> Result<()> {
                match ty {
                    InterfaceType::$ty => Ok(()),
                    other => bail!("expected `{}` found `{}`", desc(&InterfaceType::$ty), desc(other))
                }
            }
        }

        unsafe impl Lower for $primitive {
            fn lower<T>(
                &self,
                _store: &mut StoreContextMut<T>,
                _options: &Options,
                dst: &mut MaybeUninit<Self::Lower>,
            ) -> Result<()> {
                dst.write(ValRaw::$field(*self as $field));
                Ok(())
            }

            fn store<T>(&self, memory: &mut MemoryMut<'_, T>, offset: usize) -> Result<()> {
                debug_assert!(offset % Self::SIZE32 == 0);
                *memory.get(offset) = self.to_le_bytes();
                Ok(())
            }
        }

        unsafe impl Lift for $primitive {
            #[inline]
            fn lift(_store: &StoreOpaque, _options: &Options, src: &Self::Lower) -> Result<Self> {
                Ok(src.$get() as $primitive)
            }

            #[inline]
            fn load(_mem: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
                debug_assert!((bytes.as_ptr() as usize) % Self::SIZE32 == 0);
                Ok($primitive::from_le_bytes(bytes.try_into().unwrap()))
            }
        }
    )*)
}

integers! {
    i8 = S8 in i32/get_i32,
    u8 = U8 in u32/get_u32,
    i16 = S16 in i32/get_i32,
    u16 = U16 in u32/get_u32,
    i32 = S32 in i32/get_i32,
    u32 = U32 in u32/get_u32,
    i64 = S64 in i64/get_i64,
    u64 = U64 in u64/get_u64,
}

macro_rules! floats {
    ($($float:ident/$get_float:ident = $ty:ident)*) => ($(const _: () = {
        /// All floats in-and-out of the canonical abi always have their nan
        /// payloads canonicalized. conveniently the `NAN` constant in rust has
        /// the same representation as canonical nan, so we can use that for the
        /// nan value.
        #[inline]
        fn canonicalize(float: $float) -> $float {
            if float.is_nan() {
                $float::NAN
            } else {
                float
            }
        }

        unsafe impl ComponentType for $float {
            type Lower = ValRaw;

            const SIZE32: usize = mem::size_of::<$float>();

            // note that like integers size is used here instead of alignment to
            // respect the canonical abi, not host platforms.
            const ALIGN32: u32 = mem::size_of::<$float>() as u32;

            fn typecheck(ty: &InterfaceType, _types: &ComponentTypes) -> Result<()> {
                match ty {
                    InterfaceType::$ty => Ok(()),
                    other => bail!("expected `{}` found `{}`", desc(&InterfaceType::$ty), desc(other))
                }
            }
        }

        unsafe impl Lower for $float {
            fn lower<T>(
                &self,
                _store: &mut StoreContextMut<T>,
                _options: &Options,
                dst: &mut MaybeUninit<Self::Lower>,
            ) -> Result<()> {
                dst.write(ValRaw::$float(canonicalize(*self).to_bits()));
                Ok(())
            }

            fn store<T>(&self, memory: &mut MemoryMut<'_, T>, offset: usize) -> Result<()> {
                debug_assert!(offset % Self::SIZE32 == 0);
                let ptr = memory.get(offset);
                *ptr = canonicalize(*self).to_bits().to_le_bytes();
                Ok(())
            }
        }

        unsafe impl Lift for $float {
            #[inline]
            fn lift(_store: &StoreOpaque, _options: &Options, src: &Self::Lower) -> Result<Self> {
                Ok(canonicalize($float::from_bits(src.$get_float())))
            }

            #[inline]
            fn load(_mem: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
                debug_assert!((bytes.as_ptr() as usize) % Self::SIZE32 == 0);
                Ok(canonicalize($float::from_le_bytes(bytes.try_into().unwrap())))
            }
        }
    };)*)
}

floats! {
    f32/get_f32 = Float32
    f64/get_f64 = Float64
}

unsafe impl ComponentType for bool {
    type Lower = ValRaw;

    const SIZE32: usize = 1;
    const ALIGN32: u32 = 1;

    fn typecheck(ty: &InterfaceType, _types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::Bool => Ok(()),
            other => bail!("expected `bool` found `{}`", desc(other)),
        }
    }
}

unsafe impl Lower for bool {
    fn lower<T>(
        &self,
        _store: &mut StoreContextMut<T>,
        _options: &Options,
        dst: &mut MaybeUninit<Self::Lower>,
    ) -> Result<()> {
        dst.write(ValRaw::i32(*self as i32));
        Ok(())
    }

    fn store<T>(&self, memory: &mut MemoryMut<'_, T>, offset: usize) -> Result<()> {
        debug_assert!(offset % Self::SIZE32 == 0);
        memory.get::<1>(offset)[0] = *self as u8;
        Ok(())
    }
}

unsafe impl Lift for bool {
    #[inline]
    fn lift(_store: &StoreOpaque, _options: &Options, src: &Self::Lower) -> Result<Self> {
        match src.get_i32() {
            0 => Ok(false),
            _ => Ok(true),
        }
    }

    #[inline]
    fn load(_mem: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
        match bytes[0] {
            0 => Ok(false),
            _ => Ok(true),
        }
    }
}

unsafe impl ComponentType for char {
    type Lower = ValRaw;

    const SIZE32: usize = 4;
    const ALIGN32: u32 = 4;

    fn typecheck(ty: &InterfaceType, _types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::Char => Ok(()),
            other => bail!("expected `char` found `{}`", desc(other)),
        }
    }
}

unsafe impl Lower for char {
    fn lower<T>(
        &self,
        _store: &mut StoreContextMut<T>,
        _options: &Options,
        dst: &mut MaybeUninit<Self::Lower>,
    ) -> Result<()> {
        dst.write(ValRaw::u32(u32::from(*self)));
        Ok(())
    }

    fn store<T>(&self, memory: &mut MemoryMut<'_, T>, offset: usize) -> Result<()> {
        debug_assert!(offset % Self::SIZE32 == 0);
        *memory.get::<4>(offset) = u32::from(*self).to_le_bytes();
        Ok(())
    }
}

unsafe impl Lift for char {
    #[inline]
    fn lift(_store: &StoreOpaque, _options: &Options, src: &Self::Lower) -> Result<Self> {
        Ok(char::try_from(src.get_u32())?)
    }

    #[inline]
    fn load(_memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
        debug_assert!((bytes.as_ptr() as usize) % Self::SIZE32 == 0);
        let bits = u32::from_le_bytes(bytes.try_into().unwrap());
        Ok(char::try_from(bits)?)
    }
}

// Note that this is similar to `ComponentType for WasmStr` except it can only
// be used for lowering, not lifting.
unsafe impl ComponentType for str {
    type Lower = [ValRaw; 2];

    const SIZE32: usize = 8;
    const ALIGN32: u32 = 4;

    fn typecheck(ty: &InterfaceType, _types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::String => Ok(()),
            other => bail!("expected `string` found `{}`", desc(other)),
        }
    }
}

unsafe impl Lower for str {
    fn lower<T>(
        &self,
        store: &mut StoreContextMut<T>,
        options: &Options,
        dst: &mut MaybeUninit<[ValRaw; 2]>,
    ) -> Result<()> {
        let (ptr, len) = lower_string(&mut MemoryMut::new(store.as_context_mut(), options), self)?;
        // See "WRITEPTR64" above for why this is always storing a 64-bit
        // integer.
        map_maybe_uninit!(dst[0]).write(ValRaw::i64(ptr as i64));
        map_maybe_uninit!(dst[1]).write(ValRaw::i64(len as i64));
        Ok(())
    }

    fn store<T>(&self, mem: &mut MemoryMut<'_, T>, offset: usize) -> Result<()> {
        debug_assert!(offset % (Self::ALIGN32 as usize) == 0);
        let (ptr, len) = lower_string(mem, self)?;
        // FIXME: needs memory64 handling
        *mem.get(offset + 0) = (ptr as i32).to_le_bytes();
        *mem.get(offset + 4) = (len as i32).to_le_bytes();
        Ok(())
    }
}

fn lower_string<T>(mem: &mut MemoryMut<'_, T>, string: &str) -> Result<(usize, usize)> {
    match mem.string_encoding() {
        StringEncoding::Utf8 => {
            let ptr = mem.realloc(0, 0, 1, string.len())?;
            mem.as_slice_mut()[ptr..][..string.len()].copy_from_slice(string.as_bytes());
            Ok((ptr, string.len()))
        }
        StringEncoding::Utf16 => {
            let size = string.len() * 2;
            let mut ptr = mem.realloc(0, 0, 2, size)?;
            let bytes = &mut mem.as_slice_mut()[ptr..][..size];
            let mut copied = 0;
            for (u, bytes) in string.encode_utf16().zip(bytes.chunks_mut(2)) {
                let u_bytes = u.to_le_bytes();
                bytes[0] = u_bytes[0];
                bytes[1] = u_bytes[1];
                copied += 1;
            }
            if (copied * 2) < size {
                ptr = mem.realloc(ptr, size, 2, copied * 2)?;
            }
            Ok((ptr, copied))
        }
        StringEncoding::CompactUtf16 => {
            unimplemented!("compact-utf-16");
        }
    }
}

/// Representation of a string located in linear memory in a WebAssembly
/// instance.
///
/// This type is used with [`TypedFunc`], for example, when WebAssembly returns
/// a string. This type cannot be used to give a string to WebAssembly, instead
/// `&str` should be used for that (since it's coming from the host).
///
/// Note that this type represents an in-bounds string in linear memory, but it
/// does not represent a valid string (e.g. valid utf-8). Validation happens
/// when [`WasmStr::to_str`] is called.
//
// TODO: should probably expand this with examples
pub struct WasmStr {
    ptr: usize,
    len: usize,
    options: Options,
}

impl WasmStr {
    fn new(ptr: usize, len: usize, memory: &Memory<'_>) -> Result<WasmStr> {
        let byte_len = match memory.string_encoding() {
            StringEncoding::Utf8 => Some(len),
            StringEncoding::Utf16 => len.checked_mul(2),
            StringEncoding::CompactUtf16 => unimplemented!(),
        };
        match byte_len.and_then(|len| ptr.checked_add(len)) {
            Some(n) if n <= memory.as_slice().len() => {}
            _ => bail!("string pointer/length out of bounds of memory"),
        }
        Ok(WasmStr {
            ptr,
            len,
            options: *memory.options(),
        })
    }

    /// Returns the underlying string that this cursor points to.
    ///
    /// Note that this will internally decode the string from the wasm's
    /// encoding to utf-8 and additionally perform validation.
    ///
    /// The `store` provided must be the store where this string lives to
    /// access the correct memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the string wasn't encoded correctly (e.g. invalid
    /// utf-8).
    ///
    /// # Panics
    ///
    /// Panics if this string is not owned by `store`.
    //
    // TODO: should add accessors for specifically utf-8 and utf-16 that perhaps
    // in an opt-in basis don't do validation. Additionally there should be some
    // method that returns `[u16]` after validating to avoid the utf16-to-utf8
    // transcode.
    pub fn to_str<'a, T: 'a>(&self, store: impl Into<StoreContext<'a, T>>) -> Result<Cow<'a, str>> {
        self.to_str_from_store(store.into().0)
    }

    fn to_str_from_store<'a>(&self, store: &'a StoreOpaque) -> Result<Cow<'a, str>> {
        match self.options.string_encoding() {
            StringEncoding::Utf8 => self.decode_utf8(store),
            StringEncoding::Utf16 => self.decode_utf16(store),
            StringEncoding::CompactUtf16 => unimplemented!(),
        }
    }

    fn decode_utf8<'a>(&self, store: &'a StoreOpaque) -> Result<Cow<'a, str>> {
        let memory = self.options.memory(store);
        // Note that bounds-checking already happen in construction of `WasmStr`
        // so this is never expected to panic. This could theoretically be
        // unchecked indexing if we're feeling wild enough.
        Ok(str::from_utf8(&memory[self.ptr..][..self.len])?.into())
    }

    fn decode_utf16<'a>(&self, store: &'a StoreOpaque) -> Result<Cow<'a, str>> {
        let memory = self.options.memory(store);
        // See notes in `decode_utf8` for why this is panicking indexing.
        let memory = &memory[self.ptr..][..self.len * 2];
        Ok(std::char::decode_utf16(
            memory
                .chunks(2)
                .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap())),
        )
        .collect::<Result<String, _>>()?
        .into())
    }
}

// Note that this is similar to `ComponentType for str` except it can only be
// used for lifting, not lowering.
unsafe impl ComponentType for WasmStr {
    type Lower = <str as ComponentType>::Lower;

    const SIZE32: usize = <str as ComponentType>::SIZE32;
    const ALIGN32: u32 = <str as ComponentType>::ALIGN32;

    fn typecheck(ty: &InterfaceType, _types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::String => Ok(()),
            other => bail!("expected `string` found `{}`", desc(other)),
        }
    }
}

unsafe impl Lift for WasmStr {
    fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self> {
        // FIXME: needs memory64 treatment
        let ptr = src[0].get_u32();
        let len = src[1].get_u32();
        let (ptr, len) = (usize::try_from(ptr)?, usize::try_from(len)?);
        WasmStr::new(ptr, len, &Memory::new(store, options))
    }

    fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
        debug_assert!((bytes.as_ptr() as usize) % (Self::ALIGN32 as usize) == 0);
        // FIXME: needs memory64 treatment
        let ptr = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[4..].try_into().unwrap());
        let (ptr, len) = (usize::try_from(ptr)?, usize::try_from(len)?);
        WasmStr::new(ptr, len, memory)
    }
}

unsafe impl<T> ComponentType for [T]
where
    T: ComponentType,
{
    type Lower = [ValRaw; 2];

    const SIZE32: usize = 8;
    const ALIGN32: u32 = 4;

    fn typecheck(ty: &InterfaceType, types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::List(t) => T::typecheck(&types[*t], types),
            other => bail!("expected `list` found `{}`", desc(other)),
        }
    }
}

unsafe impl<T> Lower for [T]
where
    T: Lower,
{
    fn lower<U>(
        &self,
        store: &mut StoreContextMut<U>,
        options: &Options,
        dst: &mut MaybeUninit<[ValRaw; 2]>,
    ) -> Result<()> {
        let (ptr, len) = lower_list(&mut MemoryMut::new(store.as_context_mut(), options), self)?;
        // See "WRITEPTR64" above for why this is always storing a 64-bit
        // integer.
        map_maybe_uninit!(dst[0]).write(ValRaw::i64(ptr as i64));
        map_maybe_uninit!(dst[1]).write(ValRaw::i64(len as i64));
        Ok(())
    }

    fn store<U>(&self, mem: &mut MemoryMut<'_, U>, offset: usize) -> Result<()> {
        debug_assert!(offset % (Self::ALIGN32 as usize) == 0);
        let (ptr, len) = lower_list(mem, self)?;
        *mem.get(offset + 0) = (ptr as i32).to_le_bytes();
        *mem.get(offset + 4) = (len as i32).to_le_bytes();
        Ok(())
    }
}

// FIXME: this is not a memcpy for `T` where `T` is something like `u8`.
//
// Some attempts to fix this have proved not fruitful. In isolation an attempt
// was made where:
//
// * `MemoryMut` stored a `*mut [u8]` as its "last view" of memory to avoid
//   reloading the base pointer constantly. This view is reset on `realloc`.
// * The bounds-checks in `MemoryMut::get` were removed (replaced with unsafe
//   indexing)
//
// Even then though this didn't correctly vectorized for `Vec<u8>`. It's not
// entirely clear why but it appeared that it's related to reloading the base
// pointer fo memory (I guess from `MemoryMut` itself?). Overall I'm not really
// clear on what's happening there, but this is surely going to be a performance
// bottleneck in the future.
fn lower_list<T, U>(mem: &mut MemoryMut<'_, U>, list: &[T]) -> Result<(usize, usize)>
where
    T: Lower,
{
    let elem_size = T::SIZE32;
    let size = list
        .len()
        .checked_mul(elem_size)
        .ok_or_else(|| anyhow::anyhow!("size overflow copying a list"))?;
    let ptr = mem.realloc(0, 0, T::ALIGN32, size)?;
    let mut cur = ptr;
    for item in list {
        item.store(mem, cur)?;
        cur += elem_size;
    }
    Ok((ptr, list.len()))
}

/// Representation of a list of values that are owned by a WebAssembly instance.
///
/// This type is used whenever a `(list T)` is returned from a [`TypedFunc`],
/// for example. This type represents a list of values that are stored in linear
/// memory which are waiting to be read.
///
/// Note that this type represents only a valid range of bytes for the list
/// itself, it does not represent validity of the elements themselves and that's
/// performed when they're iterated.
pub struct WasmList<T> {
    ptr: usize,
    len: usize,
    options: Options,
    _marker: marker::PhantomData<T>,
}

impl<T: Lift> WasmList<T> {
    fn new(ptr: usize, len: usize, memory: &Memory<'_>) -> Result<WasmList<T>> {
        match len
            .checked_mul(T::SIZE32)
            .and_then(|len| ptr.checked_add(len))
        {
            Some(n) if n <= memory.as_slice().len() => {}
            _ => bail!("list pointer/length out of bounds of memory"),
        }
        if ptr % usize::try_from(T::ALIGN32)? != 0 {
            bail!("list pointer is not aligned")
        }
        Ok(WasmList {
            ptr,
            len,
            options: *memory.options(),
            _marker: marker::PhantomData,
        })
    }

    /// Returns the item length of this vector
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Gets the `n`th element of this list.
    ///
    /// Returns `None` if `index` is out of bounds. Returns `Some(Err(..))` if
    /// the value couldn't be decoded (it was invalid). Returns `Some(Ok(..))`
    /// if the value is valid.
    //
    // TODO: given that interface values are intended to be consumed in one go
    // should we even expose a random access iteration API? In theory all
    // consumers should be validating through the iterator.
    pub fn get(&self, store: impl AsContext, index: usize) -> Option<Result<T>> {
        self.get_from_store(store.as_context().0, index)
    }

    fn get_from_store(&self, store: &StoreOpaque, index: usize) -> Option<Result<T>> {
        if index >= self.len {
            return None;
        }
        let memory = Memory::new(store, &self.options);
        // Note that this is using panicking indexing and this is expected to
        // never fail. The bounds-checking here happened during the construction
        // of the `WasmList` itself which means these should always be in-bounds
        // (and wasm memory can only grow). This could theoretically be
        // unchecked indexing if we're confident enough and it's actually a perf
        // issue one day.
        let bytes = &memory.as_slice()[self.ptr + index * T::SIZE32..][..T::SIZE32];
        Some(T::load(&memory, bytes))
    }

    /// Returns an iterator over the elements of this list.
    ///
    /// Each item of the list may fail to decode and is represented through the
    /// `Result` value of the iterator.
    pub fn iter<'a, U: 'a>(
        &'a self,
        store: impl Into<StoreContext<'a, U>>,
    ) -> impl ExactSizeIterator<Item = Result<T>> + 'a {
        let store = store.into().0;
        (0..self.len).map(move |i| self.get_from_store(store, i).unwrap())
    }
}

macro_rules! raw_wasm_list_accessors {
    ($($i:ident)*) => ($(
        impl WasmList<$i> {
            /// Get access to the raw underlying memory for this list.
            ///
            /// This method will return a direct slice into the original wasm
            /// module's linear memory where the data for this slice is stored.
            /// This allows the embedder to have efficient access to the
            /// underlying memory if needed and avoid copies and such if
            /// desired.
            ///
            /// Note that multi-byte integers are stored in little-endian format
            /// so portable processing of this slice must be aware of the host's
            /// byte-endianness. The `from_le` constructors in the Rust standard
            /// library should be suitable for converting from little-endian.
            ///
            /// # Panics
            ///
            /// Panics if the `store` provided is not the one from which this
            /// slice originated.
            pub fn as_le_slice<'a, T: 'a>(&self, store: impl Into<StoreContext<'a, T>>) -> &'a [$i] {
                // See comments in `WasmList::get` for the panicking indexing
                let byte_size = self.len * mem::size_of::<$i>();
                let bytes = &self.options.memory(store.into().0)[self.ptr..][..byte_size];

                // The canonical ABI requires that everything is aligned to its
                // own size, so this should be an aligned array. Furthermore the
                // alignment of primitive integers for hosts should be smaller
                // than or equal to the size of the primitive itself, meaning
                // that a wasm canonical-abi-aligned list is also aligned for
                // the host. That should mean that the head/tail slices here are
                // empty.
                //
                // Also note that the `unsafe` here is needed since the type
                // we're aligning to isn't guaranteed to be valid, but in our
                // case it's just integers and bytes so this should be safe.
                unsafe {
                    let (head, body, tail) = bytes.align_to::<$i>();
                    assert!(head.is_empty() && tail.is_empty());
                    body
                }
            }
        }
    )*)
}

raw_wasm_list_accessors! {
    i8 i16 i32 i64
    u8 u16 u32 u64
}

// Note that this is similar to `ComponentType for str` except it can only be
// used for lifting, not lowering.
unsafe impl<T: ComponentType> ComponentType for WasmList<T> {
    type Lower = <[T] as ComponentType>::Lower;

    const SIZE32: usize = <[T] as ComponentType>::SIZE32;
    const ALIGN32: u32 = <[T] as ComponentType>::ALIGN32;

    fn typecheck(ty: &InterfaceType, types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::List(t) => T::typecheck(&types[*t], types),
            other => bail!("expected `list` found `{}`", desc(other)),
        }
    }
}

unsafe impl<T: Lift> Lift for WasmList<T> {
    fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self> {
        // FIXME: needs memory64 treatment
        let ptr = src[0].get_u32();
        let len = src[1].get_u32();
        let (ptr, len) = (usize::try_from(ptr)?, usize::try_from(len)?);
        WasmList::new(ptr, len, &Memory::new(store, options))
    }

    fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
        debug_assert!((bytes.as_ptr() as usize) % (Self::ALIGN32 as usize) == 0);
        // FIXME: needs memory64 treatment
        let ptr = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[4..].try_into().unwrap());
        let (ptr, len) = (usize::try_from(ptr)?, usize::try_from(len)?);
        WasmList::new(ptr, len, memory)
    }
}

/// Round `a` up to the next multiple of `align`, assuming that `align` is a power of 2.
#[inline]
pub const fn align_to(a: usize, align: u32) -> usize {
    debug_assert!(align.is_power_of_two());
    let align = align as usize;
    (a + (align - 1)) & !(align - 1)
}

/// For a field of type T starting after `offset` bytes, updates the offset to reflect the correct
/// alignment and size of T. Returns the correctly aligned offset for the start of the field.
#[inline]
pub fn next_field<T: ComponentType>(offset: &mut usize) -> usize {
    *offset = align_to(*offset, T::ALIGN32);
    let result = *offset;
    *offset += T::SIZE32;
    result
}

/// Verify that the given wasm type is a tuple with the expected fields in the right order.
fn typecheck_tuple(
    ty: &InterfaceType,
    types: &ComponentTypes,
    expected: &[fn(&InterfaceType, &ComponentTypes) -> Result<()>],
) -> Result<()> {
    match ty {
        InterfaceType::Unit if expected.len() == 0 => Ok(()),
        InterfaceType::Tuple(t) => {
            let tuple = &types[*t];
            if tuple.types.len() != expected.len() {
                if expected.len() == 0 {
                    bail!(
                        "expected unit or 0-tuple, found {}-tuple",
                        tuple.types.len(),
                    );
                }
                bail!(
                    "expected {}-tuple, found {}-tuple",
                    expected.len(),
                    tuple.types.len()
                );
            }
            for (ty, check) in tuple.types.iter().zip(expected) {
                check(ty, types)?;
            }
            Ok(())
        }
        other if expected.len() == 0 => {
            bail!("expected `unit` or 0-tuple found `{}`", desc(other))
        }
        other => bail!("expected `tuple` found `{}`", desc(other)),
    }
}

/// Verify that the given wasm type is a record with the expected fields in the right order and with the right
/// names.
pub fn typecheck_record(
    ty: &InterfaceType,
    types: &ComponentTypes,
    expected: &[(&str, fn(&InterfaceType, &ComponentTypes) -> Result<()>)],
) -> Result<()> {
    match ty {
        InterfaceType::Record(index) => {
            let fields = &types[*index].fields;

            if fields.len() != expected.len() {
                bail!(
                    "expected record of {} fields, found {} fields",
                    expected.len(),
                    fields.len()
                );
            }

            for (field, &(name, check)) in fields.iter().zip(expected) {
                check(&field.ty, types)
                    .with_context(|| format!("type mismatch for field {}", name))?;

                if field.name != name {
                    bail!("expected record field named {}, found {}", name, field.name);
                }
            }

            Ok(())
        }
        other => bail!("expected `record` found `{}`", desc(other)),
    }
}

/// Verify that the given wasm type is a variant with the expected cases in the right order and with the right
/// names.
pub fn typecheck_variant(
    ty: &InterfaceType,
    types: &ComponentTypes,
    expected: &[(&str, fn(&InterfaceType, &ComponentTypes) -> Result<()>)],
) -> Result<()> {
    match ty {
        InterfaceType::Variant(index) => {
            let cases = &types[*index].cases;

            if cases.len() != expected.len() {
                bail!(
                    "expected variant of {} cases, found {} cases",
                    expected.len(),
                    cases.len()
                );
            }

            for (case, &(name, check)) in cases.iter().zip(expected) {
                check(&case.ty, types)
                    .with_context(|| format!("type mismatch for case {}", name))?;

                if case.name != name {
                    bail!("expected variant case named {}, found {}", name, case.name);
                }
            }

            Ok(())
        }
        other => bail!("expected `variant` found `{}`", desc(other)),
    }
}

/// Verify that the given wasm type is a enum with the expected cases in the right order and with the right
/// names.
pub fn typecheck_enum(ty: &InterfaceType, types: &ComponentTypes, expected: &[&str]) -> Result<()> {
    match ty {
        InterfaceType::Enum(index) => {
            let names = &types[*index].names;

            if names.len() != expected.len() {
                bail!(
                    "expected enum of {} names, found {} names",
                    expected.len(),
                    names.len()
                );
            }

            for (name, expected) in names.iter().zip(expected) {
                if name != expected {
                    bail!("expected enum case named {}, found {}", expected, name);
                }
            }

            Ok(())
        }
        other => bail!("expected `enum` found `{}`", desc(other)),
    }
}

/// Verify that the given wasm type is a union with the expected cases in the right order.
pub fn typecheck_union(
    ty: &InterfaceType,
    types: &ComponentTypes,
    expected: &[fn(&InterfaceType, &ComponentTypes) -> Result<()>],
) -> Result<()> {
    match ty {
        InterfaceType::Union(index) => {
            let union_types = &types[*index].types;

            if union_types.len() != expected.len() {
                bail!(
                    "expected union of {} types, found {} types",
                    expected.len(),
                    union_types.len()
                );
            }

            for (index, (ty, check)) in union_types.iter().zip(expected).enumerate() {
                check(ty, types).with_context(|| format!("type mismatch for case {}", index))?;
            }

            Ok(())
        }
        other => bail!("expected `union` found `{}`", desc(other)),
    }
}

/// Verify that the given wasm type is a flags type with the expected flags in the right order and with the right
/// names.
pub fn typecheck_flags(
    ty: &InterfaceType,
    types: &ComponentTypes,
    expected: &[&str],
) -> Result<()> {
    match ty {
        InterfaceType::Flags(index) => {
            let names = &types[*index].names;

            if names.len() != expected.len() {
                bail!(
                    "expected flags type with {} names, found {} names",
                    expected.len(),
                    names.len()
                );
            }

            for (name, expected) in names.iter().zip(expected) {
                if name != expected {
                    bail!("expected flag named {}, found {}", expected, name);
                }
            }

            Ok(())
        }
        other => bail!("expected `flags` found `{}`", desc(other)),
    }
}

/// Format the specified bitflags using the specified names for debugging
pub fn format_flags(bits: &[u32], names: &[&str], f: &mut fmt::Formatter) -> fmt::Result {
    f.write_str("(")?;
    let mut wrote = false;
    for (index, name) in names.iter().enumerate() {
        if ((bits[index / 32] >> (index % 32)) & 1) != 0 {
            if wrote {
                f.write_str("|")?;
            } else {
                wrote = true;
            }

            f.write_str(name)?;
        }
    }
    f.write_str(")")
}

unsafe impl<T> ComponentType for Option<T>
where
    T: ComponentType,
{
    type Lower = TupleLower2<<u32 as ComponentType>::Lower, T::Lower>;

    const SIZE32: usize = align_to(1, T::ALIGN32) + T::SIZE32;
    const ALIGN32: u32 = T::ALIGN32;

    fn typecheck(ty: &InterfaceType, types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::Option(t) => T::typecheck(&types[*t], types),
            other => bail!("expected `option` found `{}`", desc(other)),
        }
    }
}

unsafe impl<T> Lower for Option<T>
where
    T: Lower,
{
    fn lower<U>(
        &self,
        store: &mut StoreContextMut<U>,
        options: &Options,
        dst: &mut MaybeUninit<Self::Lower>,
    ) -> Result<()> {
        match self {
            None => {
                map_maybe_uninit!(dst.A1).write(ValRaw::i32(0));
                // Note that this is unsafe as we're writing an arbitrary
                // bit-pattern to an arbitrary type, but part of the unsafe
                // contract of the `ComponentType` trait is that we can assign
                // any bit-pattern. By writing all zeros here we're ensuring
                // that the core wasm arguments this translates to will all be
                // zeros (as the canonical ABI requires).
                unsafe {
                    map_maybe_uninit!(dst.A2).as_mut_ptr().write_bytes(0u8, 1);
                }
            }
            Some(val) => {
                map_maybe_uninit!(dst.A1).write(ValRaw::i32(1));
                val.lower(store, options, map_maybe_uninit!(dst.A2))?;
            }
        }
        Ok(())
    }

    fn store<U>(&self, mem: &mut MemoryMut<'_, U>, offset: usize) -> Result<()> {
        debug_assert!(offset % (Self::ALIGN32 as usize) == 0);
        match self {
            None => {
                mem.get::<1>(offset)[0] = 0;
            }
            Some(val) => {
                mem.get::<1>(offset)[0] = 1;
                val.store(mem, offset + align_to(1, T::ALIGN32))?;
            }
        }
        Ok(())
    }
}

unsafe impl<T> Lift for Option<T>
where
    T: Lift,
{
    fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self> {
        Ok(match src.A1.get_i32() {
            0 => None,
            1 => Some(T::lift(store, options, &src.A2)?),
            _ => bail!("invalid option discriminant"),
        })
    }

    fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
        debug_assert!((bytes.as_ptr() as usize) % (Self::ALIGN32 as usize) == 0);
        let discrim = bytes[0];
        let payload = &bytes[align_to(1, T::ALIGN32)..];
        match discrim {
            0 => Ok(None),
            1 => Ok(Some(T::load(memory, payload)?)),
            _ => bail!("invalid option discriminant"),
        }
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct ResultLower<T: Copy, E: Copy> {
    tag: ValRaw,
    payload: ResultLowerPayload<T, E>,
}

#[derive(Clone, Copy)]
#[repr(C)]
union ResultLowerPayload<T: Copy, E: Copy> {
    ok: T,
    err: E,
}

unsafe impl<T, E> ComponentType for Result<T, E>
where
    T: ComponentType,
    E: ComponentType,
{
    type Lower = ResultLower<T::Lower, E::Lower>;

    const SIZE32: usize = align_to(1, Self::ALIGN32)
        + if T::SIZE32 > E::SIZE32 {
            T::SIZE32
        } else {
            E::SIZE32
        };
    const ALIGN32: u32 = if T::ALIGN32 > E::ALIGN32 {
        T::ALIGN32
    } else {
        E::ALIGN32
    };

    fn typecheck(ty: &InterfaceType, types: &ComponentTypes) -> Result<()> {
        match ty {
            InterfaceType::Expected(r) => {
                let expected = &types[*r];
                T::typecheck(&expected.ok, types)?;
                E::typecheck(&expected.err, types)?;
                Ok(())
            }
            other => bail!("expected `expected` found `{}`", desc(other)),
        }
    }
}

unsafe impl<T, E> Lower for Result<T, E>
where
    T: Lower,
    E: Lower,
{
    fn lower<U>(
        &self,
        store: &mut StoreContextMut<U>,
        options: &Options,
        dst: &mut MaybeUninit<Self::Lower>,
    ) -> Result<()> {
        // Start out by zeroing out the payload. This will ensure that if either
        // arm doesn't initialize some values then everything is still
        // deterministically set.
        //
        // Additionally, this initialization of zero means that the specific
        // types written by each `lower` call below on each arm still has the
        // correct value even when "joined" with the other arm.
        //
        // Finally note that this is required by the canonical ABI to some
        // degree where if the `Ok` arm initializes fewer values than the `Err`
        // arm then all the remaining values must be initialized to zero, and
        // that's what this does.
        unsafe {
            map_maybe_uninit!(dst.payload)
                .as_mut_ptr()
                .write_bytes(0u8, 1);
        }

        match self {
            Ok(e) => {
                map_maybe_uninit!(dst.tag).write(ValRaw::i32(0));
                e.lower(store, options, map_maybe_uninit!(dst.payload.ok))?;
            }
            Err(e) => {
                map_maybe_uninit!(dst.tag).write(ValRaw::i32(1));
                e.lower(store, options, map_maybe_uninit!(dst.payload.err))?;
            }
        }
        Ok(())
    }

    fn store<U>(&self, mem: &mut MemoryMut<'_, U>, offset: usize) -> Result<()> {
        debug_assert!(offset % (Self::ALIGN32 as usize) == 0);
        match self {
            Ok(e) => {
                mem.get::<1>(offset)[0] = 0;
                e.store(mem, offset + align_to(1, Self::ALIGN32))?;
            }
            Err(e) => {
                mem.get::<1>(offset)[0] = 1;
                e.store(mem, offset + align_to(1, Self::ALIGN32))?;
            }
        }
        Ok(())
    }
}

unsafe impl<T, E> Lift for Result<T, E>
where
    T: Lift,
    E: Lift,
{
    fn lift(store: &StoreOpaque, options: &Options, src: &Self::Lower) -> Result<Self> {
        // Note that this implementation specifically isn't trying to actually
        // reinterpret or alter the bits of `lower` depending on which variant
        // we're lifting. This ends up all working out because the value is
        // stored in little-endian format.
        //
        // When stored in little-endian format the `{T,E}::Lower`, when each
        // individual `ValRaw` is read, means that if an i64 value, extended
        // from an i32 value, was stored then when the i32 value is read it'll
        // automatically ignore the upper bits.
        //
        // This "trick" allows us to seamlessly pass through the `Self::Lower`
        // representation into the lifting/lowering without trying to handle
        // "join"ed types as per the canonical ABI. It just so happens that i64
        // bits will naturally be reinterpreted as f64. Additionally if the
        // joined type is i64 but only the lower bits are read that's ok and we
        // don't need to validate the upper bits.
        //
        // This is largely enabled by WebAssembly/component-model#35 where no
        // validation needs to be performed for ignored bits and bytes here.
        Ok(match src.tag.get_i32() {
            0 => Ok(unsafe { T::lift(store, options, &src.payload.ok)? }),
            1 => Err(unsafe { E::lift(store, options, &src.payload.err)? }),
            _ => bail!("invalid expected discriminant"),
        })
    }

    fn load(memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
        debug_assert!((bytes.as_ptr() as usize) % (Self::ALIGN32 as usize) == 0);
        let align = Self::ALIGN32;
        let discrim = bytes[0];
        let payload = &bytes[align_to(1, align)..];
        match discrim {
            0 => Ok(Ok(T::load(memory, &payload[..T::SIZE32])?)),
            1 => Ok(Err(E::load(memory, &payload[..E::SIZE32])?)),
            _ => bail!("invalid expected discriminant"),
        }
    }
}

macro_rules! impl_component_ty_for_tuples {
    ($n:tt $($t:ident)*) => {paste::paste!{
        #[allow(non_snake_case)]
        #[doc(hidden)]
        #[derive(Clone, Copy)]
        #[repr(C)]
        pub struct [<TupleLower$n>]<$($t),*> {
            $($t: $t,)*
            _align_tuple_lower0_correctly: [ValRaw; 0],
        }

        #[allow(non_snake_case)]
        unsafe impl<$($t,)*> ComponentType for ($($t,)*)
            where $($t: ComponentType),*
        {
            type Lower = [<TupleLower$n>]<$($t::Lower),*>;

            const SIZE32: usize = {
                let mut _size = 0;
                $(
                    _size = align_to(_size, $t::ALIGN32);
                    _size += $t::SIZE32;
                )*
                _size
            };

            const ALIGN32: u32 = {
                let mut _align = 1;
                $(if $t::ALIGN32 > _align {
                    _align = $t::ALIGN32;
                })*
                _align
            };

            fn typecheck(
                ty: &InterfaceType,
                types: &ComponentTypes,
            ) -> Result<()> {
                typecheck_tuple(ty, types, &[$($t::typecheck),*])
            }
        }

        #[allow(non_snake_case)]
        unsafe impl<$($t,)*> Lower for ($($t,)*)
            where $($t: Lower),*
        {
            fn lower<U>(
                &self,
                _store: &mut StoreContextMut<U>,
                _options: &Options,
                _dst: &mut MaybeUninit<Self::Lower>,
            ) -> Result<()> {
                let ($($t,)*) = self;
                $($t.lower(_store, _options, map_maybe_uninit!(_dst.$t))?;)*
                Ok(())
            }

            fn store<U>(&self, _memory: &mut MemoryMut<'_, U>, mut _offset: usize) -> Result<()> {
                debug_assert!(_offset % (Self::ALIGN32 as usize) == 0);
                let ($($t,)*) = self;
                $($t.store(_memory, next_field::<$t>(&mut _offset))?;)*
                Ok(())
            }
        }

        #[allow(non_snake_case)]
        unsafe impl<$($t,)*> Lift for ($($t,)*)
            where $($t: Lift),*
        {
            fn lift(_store: &StoreOpaque, _options: &Options, _src: &Self::Lower) -> Result<Self> {
                Ok(($($t::lift(_store, _options, &_src.$t)?,)*))
            }

            fn load(_memory: &Memory<'_>, bytes: &[u8]) -> Result<Self> {
                debug_assert!((bytes.as_ptr() as usize) % (Self::ALIGN32 as usize) == 0);
                let mut _offset = 0;
                $(let $t = $t::load(_memory, &bytes[next_field::<$t>(&mut _offset)..][..$t::SIZE32])?;)*
                Ok(($($t,)*))
            }
        }

        #[allow(non_snake_case)]
        unsafe impl<$($t,)*> ComponentParams for ($($t,)*)
            where $($t: ComponentType),*
        {
            fn typecheck_params(
                params: &[(Option<String>, InterfaceType)],
                _types: &ComponentTypes,
            ) -> Result<()> {
                if params.len() != $n {
                    bail!("expected {} types, found {}", $n, params.len());
                }
                let mut params = params.iter().map(|i| &i.1);
                $($t::typecheck(params.next().unwrap(), _types)?;)*
                debug_assert!(params.next().is_none());
                Ok(())
            }
        }

    }};
}

for_each_function_signature!(impl_component_ty_for_tuples);

fn desc(ty: &InterfaceType) -> &'static str {
    match ty {
        InterfaceType::U8 => "u8",
        InterfaceType::S8 => "s8",
        InterfaceType::U16 => "u16",
        InterfaceType::S16 => "s16",
        InterfaceType::U32 => "u32",
        InterfaceType::S32 => "s32",
        InterfaceType::U64 => "u64",
        InterfaceType::S64 => "s64",
        InterfaceType::Float32 => "f32",
        InterfaceType::Float64 => "f64",
        InterfaceType::Unit => "unit",
        InterfaceType::Bool => "bool",
        InterfaceType::Char => "char",
        InterfaceType::String => "string",
        InterfaceType::List(_) => "list",
        InterfaceType::Tuple(_) => "tuple",
        InterfaceType::Option(_) => "option",
        InterfaceType::Expected(_) => "expected",

        InterfaceType::Record(_) => "record",
        InterfaceType::Variant(_) => "variant",
        InterfaceType::Flags(_) => "flags",
        InterfaceType::Enum(_) => "enum",
        InterfaceType::Union(_) => "union",
    }
}
