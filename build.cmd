@echo off
rem Builds HQ inside the VS 2022 Build Tools environment. Newer Visual Studio installs on this
rem machine lack the desktop x64 CRT libs (msvcrt.lib), and rustc picks the newest one by default.
setlocal
set "VCVARS=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if exist "%VCVARS%" call "%VCVARS%" >nul
cargo build %*
