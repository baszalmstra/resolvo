#pragma once

#include "resolvo_vector_internal.h"
#include <atomic>
#include <algorithm>
#include <initializer_list>
#include <memory>

namespace resolvo {

template<typename T>
struct Vector {
    /// Constucts a new empty vector.
    Vector()
        : inner(const_cast<Header *>(reinterpret_cast<const Vector*>(
                cbindgen_private::resolvo_vector_empty())))
    {
    }

    /// Creates a new vector that holds all the elements of the given std::initializer_list.
    Vector(std::initializer_list<T> args)
        : Vector(Vector::with_capacity(args.size()))
    {
        auto new_data = reinterpret_cast<T *>(inner + 1);
        auto input_it = args.begin();
        for (std::size_t i = 0; i < args.size(); ++i, ++input_it) {
            new (new_data + i) T(*input_it);
            inner->size++;
        }
    }

    /// Creates a vector of a given size, with default-constructed data.
    explicit Vector(size_t size) : Vector(Vector::with_capacity(size))
    {
        auto new_data = reinterpret_cast<T *>(inner + 1);
        for (std::size_t i = 0; i < size; ++i) {
            new (new_data + i) T();
            inner->size++;
        }
    }

    /// Creates a vector of a given size, initialized with copies of `value`.
    explicit Vector(size_t size, const T &value)
        : Vector(Vector::with_capacity(size))
    {
        auto new_data = reinterpret_cast<T *>(inner + 1);
        for (std::size_t i = 0; i < size; ++i) {
            new (new_data + i) T(value);
            inner->size++;
        }
    }

    /// Constructs the container with the contents of the range `[first, last)`.
    template<class InputIt>
    Vector(InputIt first, InputIt last)
        : Vector(Vector::with_capacity(std::distance(first, last)))
    {
        std::uninitialized_copy(first, last, begin());
        inner->size = inner->capacity;
    }

    /// Creates a new vector by copying the contents of another vector.
    /// Internally this function simplify increments the reference count of the
    /// other vector. Therefore no actual data is copied.
    Vector(const Vector &other) : inner(other.inner)
    {
        if (inner->refcount > 0) {
            ++inner->refcount;
        }
    }

    /// Destroys this vector. The underlying data is destroyed if no other
    /// vector references it.
    ~Vector() { drop(); }

    /// Assigns the data of \a other to this vector and returns a reference to this vector.
    Vector &operator=(const Vector &other)
    {
        if (other.inner == inner) {
            return *this;
        }
        drop();
        inner = other.inner;
        if (inner->refcount > 0) {
            ++inner->refcount;
        }
        return *this;
    }
    /// Move-assign's \a other to this vector and returns a reference to this vector.
    Vector &operator=(Vector &&other)
    {
        std::swap(inner, other.inner);
        return *this;
    }

private:
    struct Header
    {
        std::atomic<std::intptr_t> refcount;
        std::size_t size;
        std::size_t capacity;
    };
    static_assert(alignof(T) <= alignof(Header),
                  "Not yet supported because we would need to add padding");
    Header *inner;
    explicit Vector(Header *inner) : inner(inner) { }
}

}