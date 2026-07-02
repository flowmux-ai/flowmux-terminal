/*
 * Copyright (c) 2024 - 2026 ThorVG project. All rights reserved.

 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:

 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.

 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

#ifndef _TVG_LOCK_H_
#define _TVG_LOCK_H_

#include "tvgTaskScheduler.h"

#ifdef THORVG_THREAD_SUPPORT
    #include <mutex>
#endif

namespace tvg
{
#ifdef THORVG_THREAD_SUPPORT
    struct Key
    {
        std::mutex mtx;
    };

    struct StrictKey : Key
    {
    };

    struct ScopedLock
    {
        Key* key = nullptr;

        ScopedLock(Key& k)
        {
            if (TaskScheduler::threads() > 0) {
                k.mtx.lock();
                key = &k;
            }
        }

        ScopedLock(StrictKey& k)
        {
            k.mtx.lock();
            key = &k;
        }

        ~ScopedLock()
        {
            if (key) key->mtx.unlock();
        }
    };
#else //THORVG_THREAD_SUPPORT
    // Single-threaded build: all lock primitives collapse to empty
    // structs / no-ops so that static-storage `Key` / `StrictKey`
    // globals do not pull in `<mutex>`, `pthread_*`, or any
    // libstdc++ thread-safe-static machinery.  Without this fix the
    // pre-`main()` C++ static initialisers in `tvgSwMemPool.cpp` etc.
    // construct `std::mutex` objects that in turn call into pthread
    // stubs and abort on bare-metal targets.  (This matches how LVGL's
    // vendored copy of ThorVG models the no-threads case.)
    struct Key {};
    struct StrictKey {};

    struct ScopedLock
    {
        ScopedLock(Key&) {}
        ScopedLock(StrictKey&) {}
    };
#endif //THORVG_THREAD_SUPPORT
}

#endif //_TVG_LOCK_H_

