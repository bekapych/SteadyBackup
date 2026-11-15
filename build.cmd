@echo off
rem Full build: service, GUI, MSI. Logs: build-logs\summary.txt
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\build.ps1" %*
echo.
echo Exit code: %ERRORLEVEL%
pause
