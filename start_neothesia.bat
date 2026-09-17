@echo off
cd /d "%~dp0"
set WGPU_BACKEND=dx12
set RUST_LOG=info,wgpu_hal=error,oxisynth=error
"%~dp0neothesia.exe"
pause
