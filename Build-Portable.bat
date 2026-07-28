@echo off
setlocal

rem Run from the project directory even when launched from Explorer or elsewhere.
pushd "%~dp0" || exit /b 1

powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0Build-Windows.ps1"
set "BUILD_EXIT_CODE=%ERRORLEVEL%"

popd

if not "%BUILD_EXIT_CODE%"=="0" (
  echo.
  echo Venice Media Local portable build failed with exit code %BUILD_EXIT_CODE%.
  exit /b %BUILD_EXIT_CODE%
)

echo.
echo Venice Media Local portable build is ready in release\portable\.
exit /b 0
