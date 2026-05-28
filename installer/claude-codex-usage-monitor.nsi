; NSIS installer script for Claude & Codex Usage Monitor
; Per-user install (no admin), Start Menu shortcut, Add/Remove Programs entry,
; and an optional "launch after install" checkbox on the finish page.
;
; Build (from the repository root, after `apt install nsis`):
;   makensis installer/claude-codex-usage-monitor.nsi
; Output: installer/claude-codex-usage-monitor-setup.exe

Unicode true

!include "MUI2.nsh"
!include "FileFunc.nsh"

!define APP_NAME "Claude & Codex Usage Monitor"
!define APP_ID "ClaudeCodexUsageMonitor"
!define PUBLISHER "PIARY Co.Ltd"
!define VERSION "1.0.0"
!define EXE_NAME "claude-codex-usage-monitor.exe"
!define EXE_SOURCE "..\target\x86_64-pc-windows-gnu\release\${EXE_NAME}"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${APP_NAME}"
OutFile "claude-codex-usage-monitor-setup.exe"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\${APP_ID}"
InstallDirRegKey HKCU "Software\${APP_ID}" "InstallDir"
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "${APP_NAME} Installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Copyright (C) 2026 ${PUBLISHER}"

!define MUI_ICON "..\src\icons\icon.ico"
!define MUI_UNICON "..\src\icons\icon.ico"

; Finish page: offer to launch the app
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE_NAME}"
!define MUI_FINISHPAGE_RUN_TEXT "Launch ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Install"
  ; Stop a running instance so the executable is not locked.
  nsExec::ExecToLog 'taskkill /F /IM "${EXE_NAME}"'
  Pop $0

  SetOutPath "$INSTDIR"
  File "${EXE_SOURCE}"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${EXE_NAME}"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\${APP_ID}" "InstallDir" "$INSTDIR"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${EXE_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" "$\"$INSTDIR\uninstall.exe$\" /S"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  ; Stop a running instance before removing files.
  nsExec::ExecToLog 'taskkill /F /IM "${EXE_NAME}"'
  Pop $0

  Delete "$INSTDIR\${EXE_NAME}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  ; Remove the auto-start entry the app may have created via its menu.
  DeleteRegValue HKCU "${RUN_KEY}" "${APP_ID}"

  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\${APP_ID}"
SectionEnd
