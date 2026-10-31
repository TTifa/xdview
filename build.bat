@echo off
rem ---------------------------------------------------------------------------
rem  build.bat - 用 MSVC 环境编译 xdview（默认 release，可传 debug 参数）。
rem
rem  IMPORTANT: 本文件必须以 GBK(ANSI) + CRLF 保存！
rem  若以 UTF-8 保存，中文注释会被 cmd 按 GBK 解析打碎，脚本会静默失败。
rem
rem  为什么需要这个脚本：
rem  1) Git for Windows 的 PATH 里有 GNU 的 link.exe / find.exe，会被误当成
rem     MSVC 工具而报错，所以 PATH 里要让 MSVC 目录排最前。
rem  2) 不使用 vcvars64.bat：它内部用 reg.exe 查询 Windows SDK，在 reg.exe
rem     被安全策略禁用的环境里会半途失败（cl 可用但 LIB 缺 SDK），链接报
rem     LNK1181 找不到 kernel32.lib。这里直接按 VS 安装目录取 MSVC/SDK
rem     路径，手动设置 PATH / INCLUDE / LIB。
rem
rem  前置条件：Visual Studio Build Tools 勾选「使用 C++ 的桌面开发」。
rem ---------------------------------------------------------------------------
setlocal enabledelayedexpansion

where cargo >nul 2>nul
if errorlevel 1 goto :err_cargo

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" goto :err_vswhere

set "VSPATH="
for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSPATH=%%i"
if not defined VSPATH goto :err_novs

rem ---- 推导 MSVC 工具集目录（取版本号最大的一个）----
set "MSVCROOT=!VSPATH!\VC\Tools\MSVC"
set "MSVCVER="
for /f "delims=" %%v in ('dir /b /ad /o-n "!MSVCROOT!" 2^>nul') do if not defined MSVCVER set "MSVCVER=%%v"
if not defined MSVCVER goto :err_nomsvc

rem ---- 推导 Windows 10/11 SDK 版本（取最大的一个）----
set "KITROOT=%ProgramFiles(x86)%\Windows Kits\10"
if not exist "!KITROOT!\Lib" set "KITROOT=%ProgramFiles%\Windows Kits\10"
set "SDKVER="
for /f "delims=" %%s in ('dir /b /ad /o-n "!KITROOT!\Lib" 2^>nul') do if not defined SDKVER set "SDKVER=%%s"
if not defined SDKVER goto :err_nosdk

set "VCTOOLS=!MSVCROOT!\!MSVCVER!"
set "SDKLIB=!KITROOT!\Lib\!SDKVER!"
if not exist "!VCTOOLS!\bin\HostX64\x64\link.exe" goto :err_nolink
if not exist "!SDKLIB!\um\x64\kernel32.lib" goto :err_nok32

set "PATH=!VCTOOLS!\bin\HostX64\x64;!KITROOT!\bin\!SDKVER!\x64;%PATH%"
set "INCLUDE=!VCTOOLS!\include;!KITROOT!\Include\!SDKVER!\ucrt;!KITROOT!\Include\!SDKVER!\um;!KITROOT!\Include\!SDKVER!\shared;%INCLUDE%"
set "LIB=!VCTOOLS!\lib\x64;!SDKLIB!\ucrt\x64;!SDKLIB!\um\x64;%LIB%"
echo [i] MSVC !MSVCVER!   SDK !SDKVER!

rem ---- 旧进程占用 exe 会让链接器无法覆盖 ----
tasklist /FI "IMAGENAME eq xdview.exe" /NH 2>nul | findstr /I /C:"xdview.exe" >nul
if not errorlevel 1 goto :err_running

set "TARGET=%~1"
if "%TARGET%"=="" set "TARGET=release"

echo [i] cargo build --%TARGET%
cargo build --%TARGET%
if errorlevel 1 goto :err_build

echo.
echo [OK] 构建完成：target\%TARGET%\xdview.exe
for %%f in ("target\%TARGET%\xdview.exe") do echo      时间戳: %%~tf   大小: %%~zf 字节
echo      首次运行会自动生成 config.toml，改完再运行即可。
goto :end

:err_cargo
echo [x] 找不到 cargo，请先安装 Rust：https://rustup.rs
goto :fail
:err_vswhere
echo [x] 找不到 vswhere.exe，请安装 Visual Studio Build Tools
echo     https://visualstudio.microsoft.com/visual-cpp-build-tools/
goto :fail
:err_novs
echo [x] 没有找到带 C++ 工具的 Visual Studio 安装。
echo     请在 Visual Studio Installer 里勾选「使用 C++ 的桌面开发」工作负载。
goto :fail
:err_nomsvc
echo [x] 没找到 MSVC 工具集：!MSVCROOT!
goto :fail
:err_nosdk
echo [x] 没找到 Windows SDK：!KITROOT!\Lib
goto :fail
:err_nolink
echo [x] 没找到 link.exe：!VCTOOLS!\bin\HostX64\x64
goto :fail
:err_nok32
echo [x] SDK 缺少 kernel32.lib：!SDKLIB!\um\x64
goto :fail
:err_running
echo [x] xdview.exe 正在运行，请先关闭再构建，否则无法覆盖 exe。
goto :fail
:err_build
echo [x] 构建失败
goto :fail

:fail
pause
exit /b 1
:end
pause
endlocal
