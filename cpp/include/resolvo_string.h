#pragma once

#include <string_view>
#include "resolvo_string_internal.h"

namespace resolvo {

/// A string type that is used on both the Rust and C++ side.
///
/// This type uses implicit data sharing to make it efficient to pass around
/// copied. When cloning, a reference to the data is cloned, not the data
/// itself. The data uses Copy-on-Write semantics, so the data is only cloned
/// when it is modified.
///
/// The string data is stored as UTF8-encoded bytes, and it is always terminated
/// with a null character.
struct String {
    /// Creates an empty default constructed string.
    String() {
        cbindgen_private::resolvo_string_from_bytes(this, "", 0);
    }

    /// Creates a new String from a string view. The underlying string data is copied.
    String(std::string_view s)
    {
        cbindgen_private::resolvo_string_from_bytes(this, s.data(), s.size());
    }

    /// Creates a new String from the null-terminated string pointer `s`. The underlying
    /// string data is copied. It is assumed that the string is UTF-8 encoded.
    String(const char *s) : String(std::string_view(s)) { }

    /// Creates a new String from \a other.
    String(const String &other)
    {
        cbindgen_private::resolvo_string_clone(this, &other);
    }

     /// Destroys this String and frees the memory if this is the last instance
    /// referencing it.
    ~String()
    {
        cbindgen_private::resolvo_string_drop(this);
    }

    /// Assigns \a other to this string and returns a reference to this string.
    String &operator=(const String &other)
    {
        cbindgen_private::resolvo_string_drop(this);
        cbindgen_private::resolvo_string_clone(this, &other);
        return *this;
    }

    /// Assigns the string view \a s to this string and returns a reference to this string.
    /// The underlying string data is copied.  It is assumed that the string is UTF-8 encoded.
    String &operator=(std::string_view s)
    {
        cbindgen_private::resolvo_string_drop(this);
        cbindgen_private::resolvo_string_from_bytes(this, s.data(), s.size());
        return *this;
    }

    /// Assigns null-terminated string pointer \a s to this string and returns a reference
    /// to this string. The underlying string data is copied. It is assumed that the string
    /// is UTF-8 encoded.
    String &operator=(const char *s)
    {
        return *this = std::string_view(s);
    }

    /// Move-assigns `other` to this String instance.
    String &operator=(String &&other)
    {
        std::swap(inner, other.inner);
        return *this;
    }

private:
    void *inner;
};

}
