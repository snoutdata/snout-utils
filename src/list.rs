//! Reading a Postgres `List` of pointers.
use pgrx::pg_sys;

/// The pointers in `list`, which may be `NIL` (null).
pub unsafe fn pointers<T>(list: *mut pg_sys::List) -> impl Iterator<Item = *mut T> {
	let length = if list.is_null() {
		0
	} else {
		(unsafe { (*list).length }) as usize
	};
	(0..length).map(move |i| unsafe { (*(*list).elements.add(i)).ptr_value.cast::<T>() })
}

/// The C string held by a `String` node.
pub unsafe fn string_value(node: *mut pg_sys::Node) -> *const std::ffi::c_char {
	unsafe { (*node.cast::<pg_sys::String>()).sval }
}
