# Minimal wasi-sdk toolchain for building EXTERNAL C/C++ dependencies (e.g.
# mariadb-connector-c) for wasm32-wasip2. Unlike cmake/toolchains/wasi-sdk.cmake
# this does NOT add the duckdb-core hacks (wasi-shim.hpp force-include, the
# wasi-override netdb.h stub, DUCKDB_*/SQLITE_* defines) -- deps want the real
# wasi headers. Usage:
#   cmake -DCMAKE_TOOLCHAIN_FILE=.../wasi-sdk-deps.cmake -DWASI_SDK_PREFIX=...
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
    set(WASI_TARGET_TRIPLE "wasm32-wasip2")
  endif()
endif()

set(CMAKE_SYSTEM_NAME WASI)
set(CMAKE_SYSTEM_PROCESSOR wasm32)
set(CMAKE_SYSROOT "${WASI_SDK_PREFIX}/share/wasi-sysroot")
set(CMAKE_C_COMPILER "${WASI_SDK_PREFIX}/bin/clang" CACHE PATH "")
set(CMAKE_CXX_COMPILER "${WASI_SDK_PREFIX}/bin/clang++" CACHE PATH "")
set(CMAKE_AR "${WASI_SDK_PREFIX}/bin/llvm-ar" CACHE PATH "")
set(CMAKE_RANLIB "${WASI_SDK_PREFIX}/bin/llvm-ranlib" CACHE PATH "")

set(CMAKE_C_FLAGS
    "${CMAKE_C_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID")
set(CMAKE_CXX_FLAGS
    "${CMAKE_CXX_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -stdlib=libc++ -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID")
set(CMAKE_EXE_LINKER_FLAGS
    "${CMAKE_EXE_LINKER_FLAGS} --target=${WASI_TARGET_TRIPLE} --sysroot=${CMAKE_SYSROOT} -lwasi-emulated-mman -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid")

set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
