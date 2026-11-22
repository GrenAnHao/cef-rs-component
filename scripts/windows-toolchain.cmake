# CEF clears CMAKE_CXX_FLAGS for Ninja; directory options survive that reset.
# Decode CEF headers as UTF-8 without disabling /WX or replacing user CL flags.
add_compile_options(/utf-8)
