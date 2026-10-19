@echo off
rem ============================================================
rem  Build repo-manager (single-file Windows exe)
rem
rem  Usage:  build.cmd [version]        e.g.  build.cmd 1.2.4.1
rem
rem  The version is written to version.txt in this folder; build.rs embeds it
rem  into the exe (window title / settings page / update checks). It is kept out
rem  of Cargo.toml because that field must be valid semver and rejects 4-part
rem  numbers like 1.2.4.1. Without an argument the existing version.txt is used,
rem  and Cargo.toml's `version` is the fallback when the file is missing.
rem
rem  Target triple: x86_64-pc-windows-gnu  ->  needs a MinGW-w64 toolchain
rem  Every exit path pauses, so double-clicking keeps the output readable.
rem ============================================================
setlocal
set "RC=0"
set "NEWVER=%~1"
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

where cargo >nul 2>nul
if errorlevel 1 goto :norust

rem ---- locate MinGW-w64 (gcc.exe + dlltool.exe) ----
if exist "%~dp0tools\mingw64\bin\gcc.exe" set "MINGW=%~dp0tools\mingw64"
if not defined MINGW if exist "C:\msys64\mingw64\bin\gcc.exe" set "MINGW=C:\msys64\mingw64"
if not defined MINGW if exist "C:\tools\msys64\mingw64\bin\gcc.exe" set "MINGW=C:\tools\msys64\mingw64"
if defined MINGW set "PATH=%MINGW%\bin;%PATH%"

where gcc >nul 2>nul
if errorlevel 1 goto :nomingw
where dlltool >nul 2>nul
if errorlevel 1 goto :nomingw

rem ---- the windows-gnu std also needs the winpthreads static lib ----
if not defined MINGW goto :build
if exist "%MINGW%\lib\libpthread.a" goto :build
for /f "delims=" %%I in ('rustc --print sysroot') do set "SYSROOT=%%I"
set "PTH=%SYSROOT%\lib\rustlib\x86_64-pc-windows-gnu\lib\self-contained\libpthread.a"
if not exist "%PTH%" goto :build
echo [.] copying libpthread.a from the rustup sysroot ...
copy /y "%PTH%" "%MINGW%\lib\libpthread.a" >nul

:build
rem ---- version.txt is what build.rs embeds as the app version ----
if not defined NEWVER goto :readver
if not "%NEWVER%"=="%NEWVER: =%" goto :badver
>"%~dp0version.txt" echo %NEWVER%
echo [.] version.txt written: %NEWVER%

:readver
set "SHOWVER="
if exist "%~dp0version.txt" set /p SHOWVER=<"%~dp0version.txt"
if defined SHOWVER echo [.] embedding version V%SHOWVER%

rem ---- Windows cannot replace a running exe: warn before the link step ----
tasklist /nh /fi "imagename eq repo-manager.exe" 2>nul | find /i "repo-manager.exe" >nul
if not errorlevel 1 echo [!] repo-manager.exe is still running - close it, or the link step cannot replace the exe.

rem ---- 主程序 + 插件一起构建（插件是 workspace 里的 cdylib）----
cargo build --release --workspace
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" goto :fail
rem ---- 插件 dll 统一放仓库根的 plugin\\（不再埋进 target 里）----
if not exist "%~dp0plugin" mkdir "%~dp0plugin"
copy /y "%~dp0target\release\merge_plugin.dll" "%~dp0plugin\merge-plugin.dll" >nul
if errorlevel 1 goto :fail
copy /y "%~dp0target\release\ai_log_plugin.dll" "%~dp0plugin\ai-log-plugin.dll" >nul
if errorlevel 1 goto :fail
rem 老位置留着会盖住新目录（宿主优先认 exe 隔壁），清掉免得跑到旧 dll
if exist "%~dp0target\release\plugins" rmdir /s /q "%~dp0target\release\plugins"
echo.
echo [OK] target\release\repo-manager.exe
echo [OK] plugin\merge-plugin.dll
echo [OK] plugin\ai-log-plugin.dll
if defined SHOWVER echo      embedded version: V%SHOWVER%
dir target\release\repo-manager.exe
dir plugin\*.dll
echo.
pause
exit /b 0

:norust
set "RC=1"
echo [!] Rust not found. Install it first:
echo       winget install Rustlang.Rustup
echo       rustup default stable-x86_64-pc-windows-gnu
goto :fail

:nomingw
set "RC=1"
echo [!] MinGW-w64 not found - the windows-gnu Rust toolchain needs gcc.exe and dlltool.exe.
echo     Option 1: install MSYS2, then run  pacman -S mingw-w64-x86_64-gcc
echo               (or drop a mingw64 tree into this folder as tools\mingw64)
echo     Option 2: switch to the MSVC toolchain instead:
echo               rustup default stable-x86_64-pc-windows-msvc
goto :fail

:badver
set "RC=1"
echo [!] Version must be one token with no spaces, e.g.  build.cmd 1.2.4.1
goto :fail

:fail
echo.
echo [!] Build failed (exit code %RC%). See the messages above before closing this window.
echo.
pause
exit /b 1
