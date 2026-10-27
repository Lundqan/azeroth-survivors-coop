@echo off
cd /d "%~dp0"
warcraft-survivors.exe --host
if errorlevel 1 pause
