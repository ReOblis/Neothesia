@echo off
cd /d "%~dp0"
set WGPU_BACKEND=dx12
start "" "%~dp0neothesia.exe"
