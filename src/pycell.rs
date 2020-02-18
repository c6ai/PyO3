//! Traits and structs for `#[pyclass]`.
use crate::conversion::{AsPyPointer, FromPyPointer, ToPyObject};
use crate::pyclass_init::PyClassInitializer;
use crate::pyclass_slots::{PyClassDict, PyClassWeakRef};
use crate::type_object::{PyDowncastImpl, PyObjectLayout, PyObjectSizedLayout, PyTypeInfo};
use crate::types::PyAny;
use crate::{ffi, FromPy, PyClass, PyErr, PyNativeType, PyObject, PyResult, Python};
use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

#[doc(hidden)]
#[repr(C)]
pub struct PyCellBase<T: PyTypeInfo> {
    ob_base: T::Layout,
    borrow_flag: Cell<BorrowFlag>,
}

unsafe impl<T> PyObjectLayout<T> for PyCellBase<T>
where
    T: PyTypeInfo + PyNativeType,
    T::Layout: PyObjectSizedLayout<T>,
{
    const IS_NATIVE_TYPE: bool = true;
    unsafe fn unchecked_ref(&self) -> &T {
        &*((&self) as *const &Self as *const _)
    }
    unsafe fn unchecked_mut(&self) -> &mut T {
        &mut *((&self) as *const &Self as *const _ as *mut _)
    }
}

// This impl ensures `PyCellBase` can be a base type.
impl<T> PyObjectSizedLayout<T> for PyCellBase<T>
where
    T: PyTypeInfo + PyNativeType,
    T::Layout: PyObjectSizedLayout<T>,
{
}

/// Inner type of `PyCell` without dict slots and reference counter.
/// This struct has two usages:
/// 1. As an inner type of `PyRef` and `PyRefMut`.
/// 2. As a base class when `#[pyclass(Base)]` is specified.
#[doc(hidden)]
#[repr(C)]
pub struct PyCellInner<T: PyClass> {
    ob_base: T::BaseLayout,
    value: ManuallyDrop<UnsafeCell<T>>,
}

impl<T: PyClass> AsPyPointer for PyCellInner<T> {
    fn as_ptr(&self) -> *mut ffi::PyObject {
        (self as *const _) as *mut _
    }
}

unsafe impl<T: PyClass> PyObjectLayout<T> for PyCellInner<T> {
    const IS_NATIVE_TYPE: bool = false;
    fn get_super_or(&mut self) -> Option<&mut T::BaseLayout> {
        Some(&mut self.ob_base)
    }
    unsafe fn unchecked_ref(&self) -> &T {
        &*self.value.get()
    }
    unsafe fn unchecked_mut(&self) -> &mut T {
        &mut *self.value.get()
    }
    unsafe fn py_init(&mut self, value: T) {
        self.value = ManuallyDrop::new(UnsafeCell::new(value));
    }
    unsafe fn py_drop(&mut self, py: Python) {
        ManuallyDrop::drop(&mut self.value);
        self.ob_base.py_drop(py);
    }
}

// This impl ensures `PyCellInner` can be a base type.
impl<T: PyClass> PyObjectSizedLayout<T> for PyCellInner<T> {}

impl<T: PyClass> PyCellInner<T> {
    fn get_borrow_flag(&self) -> BorrowFlag {
        let base = (&self.ob_base) as *const _ as *const PyCellBase<T::BaseNativeType>;
        unsafe { (*base).borrow_flag.get() }
    }
    fn set_borrow_flag(&self, flag: BorrowFlag) {
        let base = (&self.ob_base) as *const _ as *const PyCellBase<T::BaseNativeType>;
        unsafe { (*base).borrow_flag.set(flag) }
    }
}

// TODO(kngwyu): Mutability example
/// `PyCell` represents the concrete layout of `T: PyClass` when it is converted
/// to a Python class.
///
/// You can use it to test your `#[pyclass]` correctly works.
///
/// ```
/// # use pyo3::prelude::*;
/// # use pyo3::{py_run, PyCell};
/// #[pyclass]
/// struct Book {
///     #[pyo3(get)]
///     name: &'static str,
///     author: &'static str,
/// }
/// let gil = Python::acquire_gil();
/// let py = gil.python();
/// let book = Book {
///     name: "The Man in the High Castle",
///     author: "Philip Kindred Dick",
/// };
/// let book_cell = PyCell::new(py, book).unwrap();
/// py_run!(py, book_cell, "assert book_cell.name[-6:] == 'Castle'");
/// ```
#[repr(C)]
pub struct PyCell<T: PyClass> {
    inner: PyCellInner<T>,
    dict: T::Dict,
    weakref: T::WeakRef,
}

impl<T: PyClass> PyCell<T> {
    /// Make new `PyCell` on the Python heap and returns the reference of it.
    pub fn new(py: Python, value: impl Into<PyClassInitializer<T>>) -> PyResult<&Self>
    where
        T::BaseLayout: crate::type_object::PyObjectSizedLayout<T::BaseType>,
    {
        unsafe {
            let initializer = value.into();
            let self_ = initializer.create_cell(py)?;
            FromPyPointer::from_owned_ptr_or_err(py, self_ as _)
        }
    }

    pub fn borrow(&self) -> PyRef<'_, T> {
        self.try_borrow().expect("Already mutably borrowed")
    }

    pub fn borrow_mut(&self) -> PyRefMut<'_, T> {
        self.try_borrow_mut().expect("Already borrowed")
    }

    pub fn try_borrow(&self) -> Result<PyRef<'_, T>, PyBorrowError> {
        let flag = self.inner.get_borrow_flag();
        if flag == BorrowFlag::HAS_MUTABLE_BORROW {
            Err(PyBorrowError { _private: () })
        } else {
            self.inner.set_borrow_flag(flag.increment());
            Ok(PyRef { inner: &self.inner })
        }
    }

    pub fn try_borrow_mut(&self) -> Result<PyRefMut<'_, T>, PyBorrowMutError> {
        if self.inner.get_borrow_flag() != BorrowFlag::UNUSED {
            Err(PyBorrowMutError { _private: () })
        } else {
            self.inner.set_borrow_flag(BorrowFlag::HAS_MUTABLE_BORROW);
            Ok(PyRefMut { inner: &self.inner })
        }
    }

    pub unsafe fn try_borrow_unguarded(&self) -> Result<&T, PyBorrowError> {
        if self.inner.get_borrow_flag() == BorrowFlag::HAS_MUTABLE_BORROW {
            Err(PyBorrowError { _private: () })
        } else {
            Ok(&*self.inner.value.get())
        }
    }

    pub unsafe fn try_borrow_mut_unguarded(&self) -> Result<&mut T, PyBorrowMutError> {
        if self.inner.get_borrow_flag() != BorrowFlag::UNUSED {
            Err(PyBorrowMutError { _private: () })
        } else {
            Ok(&mut *self.inner.value.get())
        }
    }

    pub(crate) unsafe fn internal_new(py: Python) -> PyResult<*mut Self>
    where
        T::BaseLayout: crate::type_object::PyObjectSizedLayout<T::BaseType>,
    {
        let base = T::alloc(py);
        if base.is_null() {
            return Err(PyErr::fetch(py));
        }
        let base = base as *mut PyCellBase<T::BaseNativeType>;
        (*base).borrow_flag = Cell::new(BorrowFlag::UNUSED);
        let self_ = base as *mut Self;
        (*self_).dict = T::Dict::new();
        (*self_).weakref = T::WeakRef::new();
        Ok(self_)
    }
}

unsafe impl<T: PyClass> PyObjectLayout<T> for PyCell<T> {
    const IS_NATIVE_TYPE: bool = false;
    fn get_super_or(&mut self) -> Option<&mut T::BaseLayout> {
        Some(&mut self.inner.ob_base)
    }
    unsafe fn unchecked_ref(&self) -> &T {
        self.inner.unchecked_ref()
    }
    unsafe fn unchecked_mut(&self) -> &mut T {
        self.inner.unchecked_mut()
    }
    unsafe fn py_init(&mut self, value: T) {
        self.inner.value = ManuallyDrop::new(UnsafeCell::new(value));
    }
    unsafe fn py_drop(&mut self, py: Python) {
        ManuallyDrop::drop(&mut self.inner.value);
        self.dict.clear_dict(py);
        self.weakref.clear_weakrefs(self.as_ptr(), py);
        self.inner.ob_base.py_drop(py);
    }
}

unsafe impl<T: PyClass> PyDowncastImpl for PyCell<T> {
    unsafe fn unchecked_downcast(obj: &PyAny) -> &Self {
        &*(obj.as_ptr() as *const Self)
    }
    private_impl! {}
}

impl<T: PyClass> AsPyPointer for PyCell<T> {
    fn as_ptr(&self) -> *mut ffi::PyObject {
        self.inner.as_ptr()
    }
}

impl<T: PyClass> ToPyObject for &PyCell<T> {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        unsafe { PyObject::from_borrowed_ptr(py, self.as_ptr()) }
    }
}

impl<T: PyClass + fmt::Debug> fmt::Debug for PyCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_borrow() {
            Ok(borrow) => f.debug_struct("RefCell").field("value", &borrow).finish(),
            Err(_) => {
                struct BorrowedPlaceholder;
                impl fmt::Debug for BorrowedPlaceholder {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("<borrowed>")
                    }
                }
                f.debug_struct("RefCell")
                    .field("value", &BorrowedPlaceholder)
                    .finish()
            }
        }
    }
}

pub struct PyRef<'p, T: PyClass> {
    inner: &'p PyCellInner<T>,
}

impl<'p, T> PyRef<'p, T>
where
    T: PyClass,
{
    pub fn as_super(&self) -> &T::BaseType {
        unsafe { self.inner.ob_base.unchecked_ref() }
    }
}

impl<'p, T, U> PyRef<'p, T>
where
    T: PyClass + PyTypeInfo<BaseType = U, BaseLayout = PyCellInner<U>>,
    U: PyClass,
{
    pub fn into_super(self) -> PyRef<'p, U> {
        let PyRef { inner } = self;
        std::mem::forget(self);
        PyRef {
            inner: &inner.ob_base,
        }
    }
}

impl<'p, T: PyClass> Deref for PyRef<'p, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { self.inner.unchecked_ref() }
    }
}

impl<'p, T: PyClass> Drop for PyRef<'p, T> {
    fn drop(&mut self) {
        let flag = self.inner.get_borrow_flag();
        self.inner.set_borrow_flag(flag.decrement())
    }
}

impl<'p, T: PyClass> FromPy<PyRef<'p, T>> for PyObject {
    fn from_py(pyref: PyRef<'p, T>, py: Python<'_>) -> PyObject {
        unsafe { PyObject::from_borrowed_ptr(py, pyref.inner.as_ptr()) }
    }
}

impl<'a, T: PyClass> std::convert::TryFrom<&'a PyCell<T>> for crate::PyRef<'a, T> {
    type Error = PyBorrowError;
    fn try_from(cell: &'a crate::PyCell<T>) -> Result<Self, Self::Error> {
        cell.try_borrow()
    }
}

impl<'a, T: PyClass> AsPyPointer for PyRef<'a, T> {
    fn as_ptr(&self) -> *mut ffi::PyObject {
        self.inner.as_ptr()
    }
}

impl<T: PyClass + fmt::Debug> fmt::Debug for PyRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

pub struct PyRefMut<'p, T: PyClass> {
    inner: &'p PyCellInner<T>,
}

impl<'p, T: PyClass> PyRefMut<'p, T> {
    pub fn as_super(&self) -> &T::BaseType {
        unsafe { self.inner.ob_base.unchecked_ref() }
    }
    pub fn as_super_mut(&mut self) -> &mut T::BaseType {
        unsafe { self.inner.ob_base.unchecked_mut() }
    }
}

impl<'p, T, U> PyRefMut<'p, T>
where
    T: PyClass + PyTypeInfo<BaseType = U, BaseLayout = PyCellInner<U>>,
    U: PyClass,
{
    pub fn into_super(self) -> PyRefMut<'p, U> {
        let PyRefMut { inner } = self;
        std::mem::forget(self);
        PyRefMut {
            inner: &inner.ob_base,
        }
    }
}

impl<'p, T: PyClass> Deref for PyRefMut<'p, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { self.inner.unchecked_ref() }
    }
}

impl<'p, T: PyClass> DerefMut for PyRefMut<'p, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.inner.unchecked_mut() }
    }
}

impl<'p, T: PyClass> Drop for PyRefMut<'p, T> {
    fn drop(&mut self) {
        self.inner.set_borrow_flag(BorrowFlag::UNUSED)
    }
}

impl<'p, T: PyClass> FromPy<PyRefMut<'p, T>> for PyObject {
    fn from_py(pyref: PyRefMut<'p, T>, py: Python<'_>) -> PyObject {
        unsafe { PyObject::from_borrowed_ptr(py, pyref.inner.as_ptr()) }
    }
}

impl<'a, T: PyClass> AsPyPointer for PyRefMut<'a, T> {
    fn as_ptr(&self) -> *mut ffi::PyObject {
        self.inner.as_ptr()
    }
}

impl<'a, T: PyClass> std::convert::TryFrom<&'a PyCell<T>> for crate::PyRefMut<'a, T> {
    type Error = PyBorrowMutError;
    fn try_from(cell: &'a crate::PyCell<T>) -> Result<Self, Self::Error> {
        cell.try_borrow_mut()
    }
}

impl<T: PyClass + fmt::Debug> fmt::Debug for PyRefMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*(self.deref()), f)
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
struct BorrowFlag(usize);

impl BorrowFlag {
    const UNUSED: BorrowFlag = BorrowFlag(0);
    const HAS_MUTABLE_BORROW: BorrowFlag = BorrowFlag(usize::max_value());
    const fn increment(self) -> Self {
        Self(self.0 + 1)
    }
    const fn decrement(self) -> Self {
        Self(self.0 - 1)
    }
}

pub struct PyBorrowError {
    _private: (),
}

impl fmt::Debug for PyBorrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PyBorrowError").finish()
    }
}

impl fmt::Display for PyBorrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt("Already mutably borrowed", f)
    }
}

pub struct PyBorrowMutError {
    _private: (),
}

impl fmt::Debug for PyBorrowMutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PyBorrowMutError").finish()
    }
}

impl fmt::Display for PyBorrowMutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt("Already borrowed", f)
    }
}

pyo3_exception!(PyBorrowError, crate::exceptions::Exception);
pyo3_exception!(PyBorrowMutError, crate::exceptions::Exception);
