cmake_minimum_required(VERSION 3.22)
project(senca_jni C)

# SENCA_LIB_ROOT/<ABI>/libsenca_sdk.a を README の手順（cargo build --target ...）で作っておく
add_library(senca_sdk STATIC IMPORTED)
set_target_properties(senca_sdk PROPERTIES
    IMPORTED_LOCATION ${SENCA_LIB_ROOT}/${ANDROID_ABI}/libsenca_sdk.a)

add_library(senca_jni SHARED senca_jni.c)
target_include_directories(senca_jni PRIVATE ${SENCA_SDK_DIR}/include)
target_compile_options(senca_jni PRIVATE -Wall -Wextra -Werror)
target_link_libraries(senca_jni senca_sdk log dl m)
