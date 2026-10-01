@echo off
setlocal

REM ============================================================================
REM  rustrouter production build
REM
REM  Order matters: crates/router-server embeds web/dist through rust-embed
REM  (`#[folder = "../../web/dist"]`), which compiles each asset in with
REM  include_bytes!. The backend must therefore be built AFTER the frontend, or
REM  the binary carries a stale (or missing) dashboard. Cargo enforces this too:
REM  the dist files are in router-server's dep-info, so a changed asset forces a
REM  rebuild of that crate.
REM
REM  Output: rustrouter-binary\rustrouter.exe
REM ============================================================================

cd /d "%~dp0"
set "ROOT=%CD%"
set "WEB=%ROOT%\web"
set "OUT=%ROOT%\rustrouter-binary"

echo ============================================================
echo  rustrouter production build
echo ============================================================

where node >nul 2>nul
if errorlevel 1 (
    echo [ERROR] node not found on PATH.
    exit /b 1
)
where npm >nul 2>nul
if errorlevel 1 (
    echo [ERROR] npm not found on PATH.
    exit /b 1
)
where cargo >nul 2>nul
if errorlevel 1 (
    echo [ERROR] cargo not found on PATH.
    exit /b 1
)

if not exist "%WEB%\package.json" (
    echo [ERROR] web\package.json not found - run this script from the repo root.
    exit /b 1
)

REM ---------------------------------------------------------------- frontend --
REM  `npm run build` is the whole frontend gate: vue-tsc -b, then biome check,
REM  then vite build (see web/package.json). Biome runs `check`, never
REM  `check --write`, so a build cannot rewrite source files. A non-zero exit
REM  fails the build.
echo.
echo [1/3] Building frontend ...
cd /d "%WEB%"
if errorlevel 1 (
    echo [ERROR] cannot enter "%WEB%".
    exit /b 1
)

if not exist "node_modules" (
    echo       node_modules missing, running npm ci ...
    call npm ci
    if errorlevel 1 (
        echo [ERROR] npm ci failed.
        exit /b 1
    )
)

call npm run build
if errorlevel 1 (
    echo [ERROR] frontend build failed.
    exit /b 1
)

if not exist "%WEB%\dist\index.html" (
    echo [ERROR] web\dist\index.html missing after the build.
    exit /b 1
)
echo       frontend OK - web\dist is ready

REM ----------------------------------------------------------------- backend --
echo.
echo [2/3] Building backend ^(embeds web\dist^) ...
cd /d "%ROOT%"
if errorlevel 1 (
    echo [ERROR] cannot return to "%ROOT%".
    exit /b 1
)

cargo build --release --locked -p rustrouter
if errorlevel 1 (
    echo [ERROR] cargo build failed.
    exit /b 1
)

REM The cargo config may or may not pin a target triple; resolve either layout.
set "BIN=%ROOT%\target\x86_64-pc-windows-msvc\release\rustrouter.exe"
if not exist "%BIN%" set "BIN=%ROOT%\target\release\rustrouter.exe"
if not exist "%BIN%" (
    echo [ERROR] release binary not found under target\.
    exit /b 1
)

REM -------------------------------------------------------------------- copy --
echo.
echo [3/3] Copying the binary to rustrouter-binary\ ...
if not exist "%OUT%" mkdir "%OUT%"
if errorlevel 1 (
    echo [ERROR] cannot create "%OUT%".
    exit /b 1
)

copy /y "%BIN%" "%OUT%\rustrouter.exe" >nul
if errorlevel 1 (
    echo [ERROR] copy failed.
    tasklist /FI "IMAGENAME eq rustrouter.exe" /NH 2>nul | "%SystemRoot%\System32\findstr.exe" /I "rustrouter.exe" >nul
    if not errorlevel 1 echo         rustrouter.exe is running and holds the file - stop it first ^(rustrouter stop^).
    exit /b 1
)

echo.
echo ============================================================
echo  Build complete
echo    binary: %OUT%\rustrouter.exe
for %%A in ("%OUT%\rustrouter.exe") do echo    size:   %%~zA bytes
echo ============================================================

endlocal
exit /b 0
