cmake_minimum_required(VERSION 3.28...3.30)

file(READ "${CMAKE_CURRENT_SOURCE_DIR}/buildspec.json" _buildspec)
string(JSON _name GET "${_buildspec}" name)
string(JSON _version GET "${_buildspec}" version)
string(JSON _author GET "${_buildspec}" author)
string(JSON _macos_bundle_id GET "${_buildspec}" platformConfig macos bundleId)

project(${_name} VERSION ${_version} LANGUAGES C CXX)

set(CMAKE_C_STANDARD 17)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_C_VISIBILITY_PRESET hidden)
set(CMAKE_CXX_VISIBILITY_PRESET hidden)
set(CMAKE_VISIBILITY_INLINES_HIDDEN ON)
set(CMAKE_FIND_PACKAGE_TARGETS_GLOBAL ON)

if(MSVC)
  add_compile_options(/W3 /utf-8 /permissive- /Zc:__cplusplus /Zc:preprocessor)
  add_compile_definitions(UNICODE _UNICODE _CRT_SECURE_NO_WARNINGS _CRT_NONSTDC_NO_WARNINGS)
endif()

# Fetch prebuilt obs-deps, Qt6, and build libobs/obs-frontend-api from buildspec.json
function(_setup_obs_deps)
  set(deps_dir "${CMAKE_CURRENT_SOURCE_DIR}/.deps")
  string(JSON deps_json GET "${_buildspec}" dependencies)

  if(WIN32)
    set(arch "${CMAKE_VS_PLATFORM_NAME}")
    if(NOT arch)
      set(arch "x64")
    endif()
    set(platform "windows-${arch}")
    set(prebuilt_file "windows-deps-VERSION-ARCH.zip")
    set(qt6_file "windows-deps-qt6-VERSION-ARCH.zip")
    set(obs_file "VERSION.zip")
  elseif(APPLE)
    set(arch "universal")
    set(platform "macos")
    set(prebuilt_file "macos-deps-VERSION-ARCH.tar.xz")
    set(qt6_file "macos-deps-qt6-VERSION-ARCH.tar.xz")
    set(obs_file "VERSION.tar.gz")
  endif()

  set(obs_needs_build FALSE)
  foreach(dep IN ITEMS prebuilt qt6 obs-studio)
    string(JSON dep_data GET "${deps_json}" "${dep}")
    string(JSON ver GET "${dep_data}" version)
    string(JSON hash GET "${dep_data}" hashes "${platform}")
    string(JSON base_url GET "${dep_data}" baseUrl)

    if(dep STREQUAL "prebuilt")
      set(file "${prebuilt_file}")
      set(dest "obs-deps-${ver}-${arch}")
    elseif(dep STREQUAL "qt6")
      set(file "${qt6_file}")
      set(dest "obs-deps-qt6-${ver}-${arch}")
    else()
      set(file "${obs_file}")
      set(dest "obs-studio-${ver}")
      set(obs_ver "${ver}")
      set(obs_dest "${dest}")
    endif()
    string(REPLACE "VERSION" "${ver}" file "${file}")
    string(REPLACE "ARCH" "${arch}" file "${file}")

    set(stamp "${deps_dir}/.dependency_${dep}_${arch}.sha256")
    set(prev_hash "")
    if(EXISTS "${stamp}")
      file(READ "${stamp}" prev_hash)
      string(STRIP "${prev_hash}" prev_hash)
    endif()

    if(NOT prev_hash STREQUAL "${hash}" OR NOT EXISTS "${deps_dir}/${dest}")
      if(dep STREQUAL "obs-studio")
        set(url "${base_url}/${file}")
        set(obs_needs_build TRUE)
      else()
        set(url "${base_url}/${ver}/${file}")
      endif()
      if(NOT EXISTS "${deps_dir}/${file}")
        message(STATUS "Downloading ${url}")
        file(DOWNLOAD "${url}" "${deps_dir}/${file}" EXPECTED_HASH "SHA256=${hash}" STATUS dl_status)
        list(GET dl_status 0 dl_code)
        if(NOT dl_code EQUAL 0)
          file(REMOVE "${deps_dir}/${file}")
          message(FATAL_ERROR "Failed to download ${url}: ${dl_status}")
        endif()
      endif()
      file(REMOVE_RECURSE "${deps_dir}/${dest}")
      if(dep STREQUAL "obs-studio")
        file(ARCHIVE_EXTRACT INPUT "${deps_dir}/${file}" DESTINATION "${deps_dir}")
      else()
        file(MAKE_DIRECTORY "${deps_dir}/${dest}")
        file(ARCHIVE_EXTRACT INPUT "${deps_dir}/${file}" DESTINATION "${deps_dir}/${dest}")
      endif()
      file(WRITE "${stamp}" "${hash}")
    endif()

    if(dep STREQUAL "obs-studio")
      list(APPEND CMAKE_PREFIX_PATH "${deps_dir}")
    else()
      list(APPEND CMAKE_PREFIX_PATH "${deps_dir}/${dest}")
    endif()
  endforeach()

  if(APPLE)
    execute_process(COMMAND xattr -r -d com.apple.quarantine "${deps_dir}" ERROR_QUIET)
    list(APPEND CMAKE_FRAMEWORK_PATH "${deps_dir}/Frameworks")
    set(CMAKE_FRAMEWORK_PATH "${CMAKE_FRAMEWORK_PATH}" PARENT_SCOPE)
  endif()

  list(REMOVE_DUPLICATES CMAKE_PREFIX_PATH)
  set(CMAKE_PREFIX_PATH "${CMAKE_PREFIX_PATH}" CACHE PATH "CMake prefix search path" FORCE)

  if(
    obs_needs_build
    OR (
      NOT EXISTS "${deps_dir}/cmake/libobsConfig.cmake"
      AND NOT EXISTS "${deps_dir}/lib/cmake/libobs/libobsConfig.cmake"
    )
  )
    set(obs_src "${deps_dir}/${obs_dest}")
    set(obs_bld "${obs_src}/build_${arch}")
    if(WIN32)
      set(obs_gen_args -G "${CMAKE_GENERATOR}" -A "${arch}" -DCMAKE_ENABLE_SCRIPTING=OFF)
    elseif(APPLE)
      set(obs_gen_args -G Xcode "-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64" "-DCMAKE_OSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}")
    endif()
    message(STATUS "Building OBS ${obs_ver} (${arch}) libobs & obs-frontend-api...")
    execute_process(
      COMMAND "${CMAKE_COMMAND}" --fresh -S "${obs_src}" -B "${obs_bld}" ${obs_gen_args}
        -DOBS_CMAKE_VERSION=3.0.0 -DENABLE_PLUGINS=OFF -DENABLE_FRONTEND=OFF
        -DOBS_VERSION_OVERRIDE=${obs_ver} "-DCMAKE_PREFIX_PATH=${CMAKE_PREFIX_PATH}"
      COMMAND_ERROR_IS_FATAL ANY
    )
    execute_process(
      COMMAND "${CMAKE_COMMAND}" --build "${obs_bld}" --target obs-frontend-api --config Release --parallel
      COMMAND_ERROR_IS_FATAL ANY
    )
    execute_process(
      COMMAND "${CMAKE_COMMAND}" --install "${obs_bld}" --component Development --config Release --prefix "${deps_dir}"
      COMMAND_ERROR_IS_FATAL ANY
    )
  endif()
endfunction()

if(WIN32 OR APPLE)
  _setup_obs_deps()
endif()

if(APPLE)
  set(CMAKE_MACOSX_RPATH ON)
  set(CMAKE_INSTALL_RPATH "@executable_path/../Frameworks")
  if(NOT CODESIGN_IDENTITY)
    set(CODESIGN_IDENTITY "-" CACHE STRING "macOS code signing identity" FORCE)
  endif()
endif()

# Locate libobs and obs-frontend-api
find_package(libobs QUIET)
if(TARGET OBS::libobs)
  find_package(obs-frontend-api REQUIRED)
else()
  # Linux system packages (e.g. Ubuntu PPA libobs-dev)
  find_package(LibObs REQUIRED)
  add_library(OBS::libobs ALIAS libobs)
  find_path(OBS_FRONTEND_INCLUDE obs-frontend-api.h PATHS /usr/include /usr/local/include PATH_SUFFIXES obs REQUIRED)
  find_library(OBS_FRONTEND_LIB obs-frontend-api PATHS /usr/lib /usr/local/lib REQUIRED)
  add_library(OBS::obs-frontend-api UNKNOWN IMPORTED)
  set_target_properties(
    OBS::obs-frontend-api
    PROPERTIES IMPORTED_LOCATION "${OBS_FRONTEND_LIB}" INTERFACE_INCLUDE_DIRECTORIES "${OBS_FRONTEND_INCLUDE}"
  )
endif()

find_package(Qt6 COMPONENTS Widgets Core REQUIRED)

# Build the Rust staticlib core (obs/rust -> tiwi_obs)
find_program(CARGO_EXECUTABLE cargo REQUIRED)
set(RUST_MANIFEST "${CMAKE_CURRENT_SOURCE_DIR}/rust/Cargo.toml")
if(DEFINED ENV{CARGO_TARGET_DIR} AND NOT "$ENV{CARGO_TARGET_DIR}" STREQUAL "")
  file(TO_CMAKE_PATH "$ENV{CARGO_TARGET_DIR}" RUST_TARGET_DIR)
else()
  set(RUST_TARGET_DIR "${CMAKE_CURRENT_SOURCE_DIR}/rust/target")
endif()

if(WIN32)
  set(TIWI_RUST_LIB "${RUST_TARGET_DIR}/release/tiwi_obs.lib")
elseif(APPLE AND CMAKE_OSX_ARCHITECTURES MATCHES "arm64" AND CMAKE_OSX_ARCHITECTURES MATCHES "x86_64")
  set(TIWI_RUST_LIB "${RUST_TARGET_DIR}/universal/libtiwi_obs.a")
else()
  set(TIWI_RUST_LIB "${RUST_TARGET_DIR}/release/libtiwi_obs.a")
endif()

if(APPLE AND CMAKE_OSX_ARCHITECTURES MATCHES "arm64" AND CMAKE_OSX_ARCHITECTURES MATCHES "x86_64")
  add_custom_target(
    tiwi_rust_core
    ALL
    COMMAND "${CARGO_EXECUTABLE}" build --manifest-path "${RUST_MANIFEST}" --release --target aarch64-apple-darwin
    COMMAND "${CARGO_EXECUTABLE}" build --manifest-path "${RUST_MANIFEST}" --release --target x86_64-apple-darwin
    COMMAND "${CMAKE_COMMAND}" -E make_directory "${RUST_TARGET_DIR}/universal"
    COMMAND
      lipo -create
      "${RUST_TARGET_DIR}/aarch64-apple-darwin/release/libtiwi_obs.a"
      "${RUST_TARGET_DIR}/x86_64-apple-darwin/release/libtiwi_obs.a"
      -output "${TIWI_RUST_LIB}"
    BYPRODUCTS "${TIWI_RUST_LIB}"
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/rust"
    COMMENT "Building Tiwi Rust universal staticlib core"
  )
else()
  add_custom_target(
    tiwi_rust_core
    ALL
    COMMAND "${CARGO_EXECUTABLE}" build --manifest-path "${RUST_MANIFEST}" --release
    BYPRODUCTS "${TIWI_RUST_LIB}"
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/rust"
    COMMENT "Building Tiwi Rust staticlib core"
  )
endif()

add_library(${PROJECT_NAME} MODULE)
add_dependencies(${PROJECT_NAME} tiwi_rust_core)
set_target_properties(${PROJECT_NAME} PROPERTIES OUTPUT_NAME "${_name}" AUTOMOC ON)

target_sources(
  ${PROJECT_NAME}
  PRIVATE
    src/plugin-main.cpp
    src/tiwi-output.hpp
    src/tiwi-output.cpp
    src/tiwi-service.hpp
    src/tiwi-service.cpp
    src/tiwi-dock.hpp
    src/tiwi-dock.cpp
)

target_include_directories(
  ${PROJECT_NAME}
  PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/rust/include" "${CMAKE_CURRENT_SOURCE_DIR}/src"
)

target_link_libraries(${PROJECT_NAME} PRIVATE OBS::libobs Qt6::Core Qt6::Widgets "${TIWI_RUST_LIB}")
if(TARGET OBS::frontend-api)
  target_link_libraries(${PROJECT_NAME} PRIVATE OBS::frontend-api)
else()
  target_link_libraries(${PROJECT_NAME} PRIVATE OBS::obs-frontend-api)
endif()

if(WIN32)
  target_link_libraries(
    ${PROJECT_NAME}
    PRIVATE ws2_32 bcrypt crypt32 ncrypt secur32 userenv ntdll iphlpapi
  )
elseif(APPLE)
  target_link_libraries(${PROJECT_NAME} PRIVATE "-framework Security" "-framework CoreFoundation")
  set_target_properties(
    ${PROJECT_NAME}
    PROPERTIES
      BUNDLE TRUE
      BUNDLE_EXTENSION plugin
      XCODE_ATTRIBUTE_PRODUCT_NAME "${_name}"
      XCODE_ATTRIBUTE_PRODUCT_BUNDLE_IDENTIFIER "${_macos_bundle_id}"
      XCODE_ATTRIBUTE_MARKETING_VERSION "${_version}"
      XCODE_ATTRIBUTE_CURRENT_PROJECT_VERSION "1"
      XCODE_ATTRIBUTE_GENERATE_INFOPLIST_FILE YES
      XCODE_ATTRIBUTE_INFOPLIST_KEY_CFBundleDisplayName "${_name}"
      XCODE_ATTRIBUTE_CODE_SIGN_IDENTITY "${CODESIGN_IDENTITY}"
  )
  file(GLOB_RECURSE _data_files "${CMAKE_CURRENT_SOURCE_DIR}/data/*")
  foreach(_file IN LISTS _data_files)
    file(RELATIVE_PATH _rel "${CMAKE_CURRENT_SOURCE_DIR}/data" "${_file}")
    get_filename_component(_rel_dir "${_rel}" DIRECTORY)
    target_sources(${PROJECT_NAME} PRIVATE "${_file}")
    set_property(SOURCE "${_file}" PROPERTY MACOSX_PACKAGE_LOCATION "Resources/${_rel_dir}")
  endforeach()
else()
  set_target_properties(${PROJECT_NAME} PROPERTIES PREFIX "")
  target_link_libraries(${PROJECT_NAME} PRIVATE pthread dl m)
endif()
