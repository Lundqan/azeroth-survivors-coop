@echo off
cd /d "%~dp0"
set /p IP=The host's IP address: 
warcraft-survivors.exe --join %IP%
if errorlevel 1 pause
