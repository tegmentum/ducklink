# IMPORTED targets for the duckdb-excel extension's deps (EXPAT + ZLIB +
# minizip-ng). Included into the fetched excel CMakeLists by
# scripts/build-libduckdb-wasm.sh (replacing its find_package calls). The
# include dirs compile the extension's C++; the actual .a libs are merged into
# libduckdb-wasi.a by the build script, so symbols resolve at the core link.
# EXPAT + ZLIB are shared with spatial-deps.cmake -> guard with NOT TARGET so
# the two includes coexist in one configure.
set(_GEO "$ENV{HOME}/git")
get_filename_component(_REPO "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)

# --- EXPAT (expat-wasm) ---------------------------------------------------
if(NOT TARGET EXPAT::EXPAT)
  add_library(EXPAT::EXPAT STATIC IMPORTED GLOBAL)
  set_target_properties(EXPAT::EXPAT PROPERTIES
    IMPORTED_LOCATION "${_GEO}/expat-wasm/build/lib/libexpat.a"
    INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/expat-wasm/deps/expat/expat/lib")
endif()
set(EXPAT_FOUND TRUE)
set(EXPAT_INCLUDE_DIRS "${_GEO}/expat-wasm/deps/expat/expat/lib")
set(EXPAT_LIBRARY "${_GEO}/expat-wasm/build/lib/libexpat.a")

# --- ZLIB (curl-wasm) -----------------------------------------------------
if(NOT TARGET ZLIB::ZLIB)
  add_library(ZLIB::ZLIB STATIC IMPORTED GLOBAL)
  set_target_properties(ZLIB::ZLIB PROPERTIES
    IMPORTED_LOCATION "${_GEO}/curl-wasm/build/zlib/lib/libz.a"
    INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/curl-wasm/build/zlib/include")
endif()
set(ZLIB_FOUND TRUE)
set(ZLIB_LIBRARIES "${_GEO}/curl-wasm/build/zlib/lib/libz.a")
set(ZLIB_INCLUDE_DIRS "${_GEO}/curl-wasm/build/zlib/include")

# --- minizip-ng (built by scripts/build-wasi-deps.sh, zlib-only) ----------
# excel includes "minizip-ng/mz_zip.h" etc., so the include root is the parent
# of the minizip-ng/ header dir.
if(NOT TARGET MINIZIP::minizip-ng)
  add_library(MINIZIP::minizip-ng STATIC IMPORTED GLOBAL)
  set_target_properties(MINIZIP::minizip-ng PROPERTIES
    IMPORTED_LOCATION "${_REPO}/build/wasi-deps/minizip/lib/libminizip-ng.a"
    INTERFACE_INCLUDE_DIRECTORIES "${_REPO}/build/wasi-deps/minizip/include")
endif()
set(minizip-ng_FOUND TRUE)
