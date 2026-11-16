# oopbot-opencl-loader, the OpenCL functions that load the vendor's libOpenCL.so on first use, for ggml's OpenCL backend
# in oopbot-llama, and oopbot-opencl-probe, which tells the app which GPU OpenCL reports.

# Khronos's OpenCL headers, fetched by Gradle's llamaSources task.
if(NOT OPENCL_HEADERS)
  message(FATAL_ERROR "OPENCL_HEADERS is not set")
endif()

add_library(oopbot-opencl-loader STATIC opencl_loader.c)
target_include_directories(oopbot-opencl-loader PUBLIC ${OPENCL_HEADERS})
target_compile_options(oopbot-opencl-loader PRIVATE -Wall -Wextra -Werror)
target_link_libraries(oopbot-opencl-loader PRIVATE log)

add_library(oopbot-opencl-probe SHARED opencl_probe.c)
target_compile_options(oopbot-opencl-probe PRIVATE -Wall -Wextra -Werror)
target_link_libraries(oopbot-opencl-probe PRIVATE oopbot-opencl-loader)
