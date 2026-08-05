; zyrisd Windows installer (NSIS). Per-user install — no admin rights needed.
; CI (.github/workflows/release.yml) copies target/release/zyrisd.exe into this directory,
; then builds it with makensis. Output: zyrisd-setup-x86_64.exe
Unicode true

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "WordFunc.nsh"
!include "StrFunc.nsh"
${Using:StrFunc} StrStr
!include "WinMessages.nsh"

!define APPNAME "zyrisd"
!define VERSION "0.1.0"
!define PUBLISHER "Attacca"

Name "${APPNAME} ${VERSION}"
OutFile "zyrisd-setup-x86_64.exe"
InstallDir "$LOCALAPPDATA\zyrisd"
InstallDirRegKey HKCU "Software\zyrisd" "InstallDir"
RequestExecutionLevel user

VIProductVersion "0.1.0.0"
VIAddVersionKey "ProductName" "zyrisd"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "Korean"
!insertmacro MUI_LANGUAGE "English"

Section "zyrisd (required)" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "zyrisd.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "Software\zyrisd" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd" "DisplayName" "zyrisd (Attacca always-on daemon)"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd" "NoRepair" 1

  ; PATH (user environment). Entries may hold %VAR%, so write it with ExpandStr.
  ReadRegStr $0 HKCU "Environment" "Path"
  ${If} $0 == ""
    WriteRegExpandStr HKCU "Environment" "Path" "$INSTDIR"
  ${Else}
    ${StrStr} $1 $0 "$INSTDIR"
    ${If} $1 == ""
      WriteRegExpandStr HKCU "Environment" "Path" "$0;$INSTDIR"
    ${EndIf}
  ${EndIf}
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
SectionEnd

Section "Connect at boot (zyrisd run)" SEC_AUTOSTART
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "zyrisd" '"$INSTDIR\zyrisd.exe" run'
SectionEnd

Section "Uninstall"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "zyrisd"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\zyrisd"
  DeleteRegKey HKCU "Software\zyrisd"

  ; Remove from PATH (prefix match, so we live with a leftover ';').
  ReadRegStr $0 HKCU "Environment" "Path"
  ${If} $0 != ""
    ${WordReplace} $0 "$INSTDIR" "" "+" $2
    ${WordReplace} $2 ";;" ";" "+" $3
    WriteRegExpandStr HKCU "Environment" "Path" "$3"
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  ${EndIf}

  Delete "$INSTDIR\zyrisd.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
SectionEnd
