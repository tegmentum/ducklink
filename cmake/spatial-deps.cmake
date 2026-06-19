# IMPORTED targets for the duckdb-spatial extension's native deps, backed by the
# wasm static libs under ~/git/*-wasm. Included into the fetched spatial
# CMakeLists by scripts/build-libduckdb-wasm.sh (replacing its find_package
# calls). Only the INTERFACE_INCLUDE_DIRECTORIES matter for compiling the
# extension's C++ — the actual .a libs are merged into libduckdb-wasi.a by the
# build script, so symbols resolve at the core link.
set(_GEO "$ENV{HOME}/git")

# --- GDAL -----------------------------------------------------------------
add_library(GDAL::GDAL STATIC IMPORTED GLOBAL)
set_target_properties(GDAL::GDAL PROPERTIES
  IMPORTED_LOCATION "${_GEO}/gdal-wasm/build/deps/gdal/libgdal.a"
  INTERFACE_INCLUDE_DIRECTORIES
    "${_GEO}/gdal-wasm/deps/gdal/port;${_GEO}/gdal-wasm/deps/gdal/gcore;${_GEO}/gdal-wasm/deps/gdal/ogr;${_GEO}/gdal-wasm/deps/gdal/ogr/ogrsf_frmts;${_GEO}/gdal-wasm/deps/gdal/alg;${_GEO}/gdal-wasm/build/deps/gdal/port;${_GEO}/gdal-wasm/build/deps/gdal/gcore")
set(GDAL_FOUND TRUE)
set(GDAL_INCLUDE_DIRS "")  # propagated via the target

# --- PROJ -----------------------------------------------------------------
add_library(PROJ::proj STATIC IMPORTED GLOBAL)
set_target_properties(PROJ::proj PROPERTIES
  IMPORTED_LOCATION "${_GEO}/proj-wasm/build_real_sqlite/deps/proj/lib/libproj.a"
  INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/proj-wasm/deps/proj/src")
set(PROJ_FOUND TRUE)

# --- GEOS -----------------------------------------------------------------
add_library(GEOS::geos_c STATIC IMPORTED GLOBAL)
set_target_properties(GEOS::geos_c PROPERTIES
  IMPORTED_LOCATION "${_GEO}/geos-wasm/lib/lib/libgeos_c.a"
  INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/geos-wasm/lib/include")
set(GEOS_FOUND TRUE)

# --- EXPAT (shared with excel-deps.cmake -> guard) ------------------------
if(NOT TARGET EXPAT::EXPAT)
add_library(EXPAT::EXPAT STATIC IMPORTED GLOBAL)
set_target_properties(EXPAT::EXPAT PROPERTIES
  IMPORTED_LOCATION "${_GEO}/expat-wasm/build/lib/libexpat.a"
  INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/expat-wasm/deps/expat/expat/lib")
endif()
set(EXPAT_FOUND TRUE)
set(EXPAT_INCLUDE_DIRS "${_GEO}/expat-wasm/deps/expat/expat/lib")
set(EXPAT_LIBRARY "${_GEO}/expat-wasm/build/lib/libexpat.a")

# --- sqlite3 (spatial uses the vcpkg `unofficial::sqlite3::sqlite3` target) ---
add_library(unofficial::sqlite3::sqlite3 STATIC IMPORTED GLOBAL)
set_target_properties(unofficial::sqlite3::sqlite3 PROPERTIES
  IMPORTED_LOCATION "${_GEO}/proj-wasm/build_real_sqlite/deps/sqlite/libsqlite3.a"
  INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/proj-wasm/deps/sqlite")
set(SQLite3_FOUND TRUE)
set(unofficial-sqlite3_FOUND TRUE)

# --- zlib (from curl-wasm; shared with excel-deps.cmake -> guard) ---------
if(NOT TARGET ZLIB::ZLIB)
add_library(ZLIB::ZLIB STATIC IMPORTED GLOBAL)
set_target_properties(ZLIB::ZLIB PROPERTIES
  IMPORTED_LOCATION "${_GEO}/curl-wasm/build/zlib/lib/libz.a"
  INTERFACE_INCLUDE_DIRECTORIES "${_GEO}/curl-wasm/build/zlib/include")
endif()
set(ZLIB_FOUND TRUE)
set(ZLIB_LIBRARIES "${_GEO}/curl-wasm/build/zlib/lib/libz.a")
set(ZLIB_INCLUDE_DIRS "${_GEO}/curl-wasm/build/zlib/include")
