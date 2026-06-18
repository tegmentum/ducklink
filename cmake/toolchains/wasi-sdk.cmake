# Configure CMake to target wasm32-wasi using wasi-sdk.
# Usage: cmake -S <duckdb_src> -B <build_dir> \
#   -DCMAKE_TOOLCHAIN_FILE=/path/to/cmake/toolchains/wasi-sdk.cmake \
#   -DWASI_SDK_PREFIX=/opt/wasi-sdk

if(NOT DEFINED WASI_SDK_PREFIX)
  if(DEFINED ENV{WASI_SDK_PREFIX})
    set(WASI_SDK_PREFIX "$ENV{WASI_SDK_PREFIX}")
  else()
    message(FATAL_ERROR "Set WASI_SDK_PREFIX to the wasi-sdk installation root")
  endif()
endif()

if(NOT DEFINED WASI_TARGET_TRIPLE)
  if(DEFINED ENV{WASI_TARGET_TRIPLE})
    set(WASI_TARGET_TRIPLE "$ENV{WASI_TARGET_TRIPLE}")
  else()
    set(WASI_TARGET_TRIPLE "wasm32-wasip1-threads")
  endif()
endif()

set(CMAKE_SYSTEM_NAME WASI)
set(CMAKE_SYSTEM_PROCESSOR wasm32)
set(CMAKE_SYSROOT "${WASI_SDK_PREFIX}/share/wasi-sysroot")

set(CMAKE_C_COMPILER "${WASI_SDK_PREFIX}/bin/clang" CACHE PATH "")
set(CMAKE_CXX_COMPILER "${WASI_SDK_PREFIX}/bin/clang++" CACHE PATH "")
set(CMAKE_AR "${WASI_SDK_PREFIX}/bin/llvm-ar" CACHE PATH "")
set(CMAKE_RANLIB "${WASI_SDK_PREFIX}/bin/llvm-ranlib" CACHE PATH "")

set(WASI_STUB_HEADER "${CMAKE_CURRENT_LIST_DIR}/wasi-shim.hpp")
set(WASI_OVERRIDE_INCLUDE_DIR "${CMAKE_CURRENT_LIST_DIR}/../wasi-override/include")

set(DUCKDB_SKIP_HTTP ON CACHE BOOL "Disable DuckDB HTTP subsystem for WASI builds")

# sqlite3.c (vendored by the sqlite_scanner extension) has a unix VFS that
# references syscalls wasi omits. SQLITE_OS_OTHER=1 drops it; we provide
# sqlite3_os_init() + a WASI VFS (cmake/sqlite-wasi-vfs/, reused from
# ~/git/sqlite-wasm). The supporting flags avoid temp-file VFS ops, the missing
# `timezone` global, and OS mutex/memstatus. These SQLITE_ macros are inert for
# every non-sqlite C file. C-only: sqlite_scanner's C++ uses opaque handles.
set(SQLITE_WASI_FLAGS "-DSQLITE_OS_OTHER=1 -DSQLITE_THREADSAFE=1 -DSQLITE_MUTEX_NOOP -DSQLITE_TEMP_STORE=2 -DSQLITE_OMIT_LOCALTIME -DSQLITE_DEFAULT_MEMSTATUS=0")
set(CMAKE_C_FLAGS
    "${CMAKE_C_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_SIGNAL -DDISABLE_DUCKDB_REMOTE_INSTALL -DDUCKDB_DISABLE_EXTENSION_LOAD -DDUCKDB_NO_THREADS -DDUCKDB_SKIP_HTTP ${SQLITE_WASI_FLAGS} -I${WASI_OVERRIDE_INCLUDE_DIR} -include${WASI_STUB_HEADER}")
# `-fwasm-exceptions` with `-wasm-use-legacy-eh=false` emits the standardized
# (try_table/throw_ref) wasm exception encoding, matching wasi-sdk-33's `eh`
# multilib and wasmtime's production `exceptions` feature. Without this, DuckDB
# throws (e.g. binder overload resolution) abort the whole module.
set(CMAKE_CXX_FLAGS
    "${CMAKE_CXX_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -stdlib=libc++ -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_SIGNAL -DDISABLE_DUCKDB_REMOTE_INSTALL -DDUCKDB_DISABLE_EXTENSION_LOAD -DDUCKDB_NO_THREADS -DDUCKDB_SKIP_HTTP -I${WASI_OVERRIDE_INCLUDE_DIR} -include${WASI_STUB_HEADER}")

set(CMAKE_EXE_LINKER_FLAGS
    "${CMAKE_EXE_LINKER_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -D_WASI_EMULATED_MMAN -lwasi-emulated-mman -D_WASI_EMULATED_SIGNAL -lwasi-emulated-signal")
set(CMAKE_SHARED_LINKER_FLAGS
    "${CMAKE_SHARED_LINKER_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -D_WASI_EMULATED_MMAN -lwasi-emulated-mman -D_WASI_EMULATED_SIGNAL -lwasi-emulated-signal")

if(WASI_TARGET_TRIPLE MATCHES "threads")
  set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -pthread")
  set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -pthread")
  set(CMAKE_EXE_LINKER_FLAGS "${CMAKE_EXE_LINKER_FLAGS} -pthread")
  set(CMAKE_SHARED_LINKER_FLAGS "${CMAKE_SHARED_LINKER_FLAGS} -pthread")
  set(CMAKE_HAVE_THREADS_LIBRARY 1)
  set(CMAKE_USE_PTHREADS_INIT 1)
else()
  # Non-threaded builds default to the single-threaded runtime.
  set(CMAKE_THREAD_LIBS_INIT "")
  set(CMAKE_HAVE_THREADS_LIBRARY 1)
  set(CMAKE_USE_PTHREADS_INIT 0)
endif()

set(CMAKE_USE_WIN32_THREADS_INIT 0)
