@echo off
rem Run cargo on Windows with MSVC and the gvsbuild GTK runtime in scope.
rem Usage: ci\win-build.cmd test --verbose
setlocal
rem -products is required: vswhere skips Build Tools without it.
pushd "%ProgramFiles(x86)%\Microsoft Visual Studio\Installer"
for /f "usebackq tokens=*" %%i in (`vswhere.exe -latest -products "*" -property installationPath`) do set "VSPATH=%%i"
popd
if not defined VSPATH (
  echo Visual Studio not found>&2
  exit /b 1
)
call "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
set "PKG_CONFIG_PATH=C:\gtk\lib\pkgconfig"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
set "PATH=C:\gtk\bin;%LIBCLANG_PATH%;%USERPROFILE%\.cargo\bin;%PATH%"
cd /d "%~dp0.."
cargo %*
