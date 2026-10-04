; NSIS installer for NyaRemoteControl for Windows: the program users open,
; the remote-control service and the command line, all in one directory.
;
; Keep this file UTF-8 *with BOM*: NSIS reads a BOM-less file in the system
; code page and rejects the Chinese strings.
;
; Built by scripts\build-release.ps1:
;   makensis /DVERSION=0.7.0 /DVI_VERSION=0.7.0.0 /DSOURCE_DIR=<dist\NyaRemoteControl> /DOUTFILE=<setup.exe> /DICON=<app.ico> installer\NyaRemoteControl.nsi
;
; Per-machine, 64-bit, Windows 10+. Remote control of this computer (the
; Windows service) is opt-in: the "允许远程控制本机" component, or /SERVICE
; for a silent install; it stays on when upgrading a computer that has it.
;
; Upgrades in place. It also takes over the separate client and server
; installations from before 0.7 (Program Files\NyaRemoteControl\Client and
; \Server): their files, shortcuts and uninstall entries go, the service is
; re-registered from here. Settings stay where they were: saved devices in
; %APPDATA%\NyaRemoteControl\client, pairing data and certificate in
; C:\ProgramData\NyaRemoteControl.
;
; Command line: /S silent, /SERVICE turn remote control on, /UPDATE an
; update (close the running program instead of asking; start it again
; afterwards unless the installer runs as SYSTEM, i.e. from the service's
; updater), /D=<dir> last.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "Pass /DVERSION=MAJOR.MINOR.PATCH"
!endif
!ifndef VI_VERSION
  !error "Pass /DVI_VERSION=MAJOR.MINOR.PATCH.0"
!endif
!ifndef SOURCE_DIR
  !error "Pass /DSOURCE_DIR=<dist\NyaRemoteControl>"
!endif
!ifndef OUTFILE
  !define OUTFILE "NyaRemoteControl_${VERSION}_x64-setup.exe"
!endif

!define APP_NAME "NyaRemoteControl"
!define APP_ID "NyaRemoteControl"
!define APP_EXE "NyaRemoteControl.exe"
!define CLI_EXE "nya-server.exe"
!define HOST_EXE "nya-server-svc.exe"
; Before 0.7: the client and the host's management program.
!define OLD_CLIENT_EXE "nya-client.exe"
!define SERVICE "NyaRemoteControl"
!define COMPANY "NyaRemoteControl"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
!define OLD_SERVER_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\NyaRemoteControl.Server"
!define OLD_CLIENT_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\NyaRemoteControl.Client"
; WebView2 Runtime (Evergreen): per-machine and per-user registrations.
!define WEBVIEW2_KEY "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WEBVIEW2_USER_KEY "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "Sections.nsh"
!include "FileFunc.nsh"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\NyaRemoteControl"
InstallDirRegKey HKLM "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel admin
ShowInstDetails show
ShowUninstDetails show
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VI_VERSION}"
VIFileVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "NyaRemoteControl"
VIAddVersionKey "CompanyName" "${COMPANY}"
VIAddVersionKey "LegalCopyright" "MIT License"
VIAddVersionKey "FileDescription" "NyaRemoteControl Setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"

!ifdef ICON
  !define MUI_ICON "${ICON}"
  !define MUI_UNICON "${ICON}"
!endif
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "将安装 ${APP_NAME} ${VERSION}：用它远程控制别的电脑，也可以开启“允许远程控制本机”让别人控制这台电脑。$\r$\n$\r$\n已有旧版本（包括以前分开安装的客户端和被控端）时会直接升级，已保存的设备、配对信息和设置都保留。$\r$\n$\r$\n点击“下一步”继续。"
!define MUI_COMPONENTSPAGE_TEXT_TOP "只用来控制别的电脑时，不需要勾选“允许远程控制本机”；以后也可以在程序的“本机”页里开启。"
!define MUI_COMPONENTSPAGE_NODESC
; The installer runs elevated; going through explorer.exe starts the program
; as the normal user (its settings live in that user's profile).
!define MUI_FINISHPAGE_RUN ""
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchAsUser
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

Var ServiceExisted
Var OldServerDir
Var OldClientDir

Function LaunchAsUser
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

; Pushes 0 when a process with this image name runs.
!macro IsRunning EXE
  nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq ${EXE}" /NH | find /I "${EXE}"'
!macroend

; Wait up to 20 s for EXE to exit, then end it.
!macro WaitOrKill EXE
  StrCpy $1 0
  ${Do}
    !insertmacro IsRunning "${EXE}"
    Pop $0
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    ${If} $1 >= 40
      nsExec::Exec 'taskkill /F /IM ${EXE}'
      Pop $0
      Sleep 1000
      ${Break}
    ${EndIf}
    Sleep 500
    IntOp $1 $1 + 1
  ${Loop}
!macroend

; Ask the user to close a running program (it holds its files open).
!macro AskToClose EXE TEXT
  ${Do}
    !insertmacro IsRunning "${EXE}"
    Pop $0
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${TEXT}请先关闭它，然后点击“重试”。" /SD IDCANCEL IDRETRY +2
    Abort
  ${Loop}
!macroend

!macro EnsureAppClosed UN
Function ${UN}EnsureAppClosed
  ; Silent (an update): the program is quitting by itself, or nobody is
  ; there to ask (the service's updater); wait for it, then make sure.
  ${If} ${Silent}
    !insertmacro WaitOrKill "${APP_EXE}"
    !insertmacro WaitOrKill "${OLD_CLIENT_EXE}"
    ; The command line; before 0.7 also the host's management GUI. The
    ; service's updater runs as nya-updater.exe and is not affected.
    nsExec::Exec 'taskkill /F /IM ${CLI_EXE}'
    Pop $0
    Return
  ${EndIf}
  !insertmacro AskToClose "${APP_EXE}" "${APP_NAME} 正在运行（可能有远程连接；也可能在屏幕右下角的托盘里，右键图标选“退出”）。"
  !insertmacro AskToClose "${OLD_CLIENT_EXE}" "旧版 NyaRemoteControl 客户端正在运行（可能有远程连接）。"
  !insertmacro AskToClose "${CLI_EXE}" "旧版被控端管理程序（${CLI_EXE}）正在运行。"
FunctionEnd
!macroend
!insertmacro EnsureAppClosed ""
!insertmacro EnsureAppClosed "un."

; Stop the service and wait for the host processes to exit (they lock the files).
!macro StopHost UN
Function ${UN}StopHost
  nsExec::Exec 'sc stop ${SERVICE}'
  Pop $0
  ; Also a development-mode instance (nya-server-svc standalone).
  StrCpy $1 0
  ${Do}
    !insertmacro IsRunning "${HOST_EXE}"
    Pop $0
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    ${If} $1 >= 30
      DetailPrint "被控端进程没有按时退出，强制结束"
      nsExec::Exec 'taskkill /F /IM ${HOST_EXE}'
      Pop $0
      Sleep 1000
      ${Break}
    ${EndIf}
    Sleep 500
    IntOp $1 $1 + 1
  ${Loop}
FunctionEnd
!macroend
!insertmacro StopHost ""
!insertmacro StopHost "un."

; Start menu and desktop shortcuts of every version, for all users and the
; installing user (0.2.0 put them in the installing user's profile).
!macro DeleteShortcuts
  ${ForEach} $9 0 1 + 1
    ${If} $9 == 1
      SetShellVarContext all
    ${Else}
      SetShellVarContext current
    ${EndIf}
    Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl.lnk"
    Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl.lnk"
    Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 客户端.lnk"
    Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 客户端.lnk"
    Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 被控端管理.lnk"
    Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 被控端.lnk"
    RMDir "$SMPROGRAMS\NyaRemoteControl"
    Delete "$DESKTOP\NyaRemoteControl.lnk"
    Delete "$DESKTOP\NyaRemoteControl 客户端.lnk"
  ${Next}
  SetShellVarContext all
!macroend

; The launcher is a WebView2 page; Windows 11 and up-to-date Windows 10 have
; the runtime, stripped-down systems may not.
Function CheckWebView2
  ReadRegStr $0 HKLM "${WEBVIEW2_KEY}" "pv"
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKCU "${WEBVIEW2_USER_KEY}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    MessageBox MB_YESNO|MB_ICONEXCLAMATION "这台电脑没有安装 Microsoft Edge WebView2 运行库，程序主界面需要它（远程控制本机的服务不需要）。$\r$\n$\r$\n是否打开下载页面（选择“常青版独立安装程序”）？安装完成后再运行程序即可。" /SD IDNO IDNO +2
    ExecShell "open" "https://developer.microsoft.com/microsoft-edge/webview2/"
  ${EndIf}
FunctionEnd

; Remove an installation from before 0.7: its files by name (the directory
; may have been chosen freely), then the directory if that emptied it.
!macro RemoveOld DIR KEY
  ${If} ${DIR} != ""
  ${AndIf} ${DIR} != $INSTDIR
    DetailPrint "删除旧版本：${DIR}"
    Delete "${DIR}\${OLD_CLIENT_EXE}"
    Delete "${DIR}\${CLI_EXE}"
    Delete "${DIR}\${HOST_EXE}"
    Delete "${DIR}\avcodec-*.dll"
    Delete "${DIR}\avutil-*.dll"
    Delete "${DIR}\swresample-*.dll"
    Delete "${DIR}\README.md"
    Delete "${DIR}\LICENSE-ViGEmClient.txt"
    Delete "${DIR}\drivers\*.*"
    RMDir "${DIR}\drivers"
    Delete "${DIR}\uninstall.exe"
    RMDir "${DIR}"
  ${EndIf}
  DeleteRegKey HKLM "${KEY}"
!macroend

Section "-程序文件" SecFiles
  SectionIn RO
  Call EnsureAppClosed
  DetailPrint "停止远程控制服务…"
  Call StopHost
  !insertmacro DeleteShortcuts

  SetOutPath "$INSTDIR"
  File /r "${SOURCE_DIR}\*.*"
  ; Files of the separate client / server packages that are not ours anymore.
  Delete "$INSTDIR\${OLD_CLIENT_EXE}"

  CreateDirectory "$SMPROGRAMS\NyaRemoteControl"
  CreateShortcut "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl.lnk" "$INSTDIR\uninstall.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${COMPANY}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  ; Pairing links (nyaremote://pair?…, from a host's 本机 page) open the program.
  WriteRegStr HKLM "Software\Classes\nyaremote" "" "URL:NyaRemoteControl 配对链接"
  WriteRegStr HKLM "Software\Classes\nyaremote" "URL Protocol" ""
  WriteRegStr HKLM "Software\Classes\nyaremote\DefaultIcon" "" "$INSTDIR\${APP_EXE},0"
  WriteRegStr HKLM "Software\Classes\nyaremote\shell\open\command" "" '"$INSTDIR\${APP_EXE}" "%1"'
SectionEnd

Section "桌面快捷方式" SecDesktop
  CreateShortcut "$DESKTOP\NyaRemoteControl.lnk" "$INSTDIR\${APP_EXE}"
SectionEnd

Section /o "允许远程控制本机（安装后台服务）" SecService
  DetailPrint "安装并启动服务（防火墙放行 UDP 和 TCP 端口、远程 Ctrl+Alt+Del 策略）…"
  nsExec::ExecToLog '"$INSTDIR\${CLI_EXE}" install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONEXCLAMATION "服务没有安装成功（代码 $0），详情见安装日志。$\r$\n可以稍后在程序的“本机”页里点“开启远程控制”重试。" /SD IDOK
  ${EndIf}
SectionEnd

Section "-收尾" SecFinish
  ; The old installations, after the service points here (SecService).
  !insertmacro RemoveOld $OldServerDir "${OLD_SERVER_KEY}"
  !insertmacro RemoveOld $OldClientDir "${OLD_CLIENT_KEY}"
  ${IfNot} ${SectionIsSelected} ${SecService}
  ${AndIf} $ServiceExisted == 0
    ; Kept off by the user although it was on: take it away cleanly.
    nsExec::ExecToLog '"$INSTDIR\${CLI_EXE}" uninstall'
    Pop $0
  ${EndIf}

  Call CheckWebView2

  ; An update (/UPDATE): start the new version as the user, unless we run as
  ; SYSTEM (the service's updater; nobody is logged in to this session).
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    ReadEnvStr $1 USERNAME
    ReadEnvStr $2 COMPUTERNAME
    ${If} $1 != "$2$$"
    ${AndIf} $1 != "SYSTEM"
      Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
    ${EndIf}
  ${EndIf}
SectionEnd

Function .onInit
  ${IfNot} ${RunningX64}
  ${OrIfNot} ${AtLeastWin10}
    MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} 需要 64 位 Windows 10 或更高版本。"
    Abort
  ${EndIf}
  SetRegView 64
  ReadRegStr $OldServerDir HKLM "${OLD_SERVER_KEY}" "InstallLocation"
  ReadRegStr $OldClientDir HKLM "${OLD_CLIENT_KEY}" "InstallLocation"
  ; Upgrade into the existing directory (InstallDirRegKey reads the 32-bit
  ; registry view, so look again in the 64-bit one); /D= still wins, except
  ; that the updater of a host before 0.7 passes its old Server directory:
  ; the new version goes to the new place.
  ReadRegStr $0 HKLM "${UNINST_KEY}" "InstallLocation"
  ${If} $INSTDIR == "$PROGRAMFILES64\NyaRemoteControl"
  ${AndIf} $0 != ""
    StrCpy $INSTDIR $0
  ${ElseIf} $INSTDIR == $OldServerDir
  ${OrIf} $INSTDIR == $OldClientDir
    StrCpy $INSTDIR "$PROGRAMFILES64\NyaRemoteControl"
    ${If} $0 != ""
      StrCpy $INSTDIR $0
    ${EndIf}
  ${EndIf}
  ; A computer that has the service keeps it (selected; still changeable).
  nsExec::Exec 'sc query ${SERVICE}'
  Pop $ServiceExisted
  ${If} $ServiceExisted == 0
    !insertmacro SelectSection ${SecService}
    ; Silent upgrades never turn it off.
    ${If} ${Silent}
      SectionSetFlags ${SecService} ${SF_SELECTED}|${SF_RO}
    ${EndIf}
  ${EndIf}
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/SERVICE" $1
  ${IfNot} ${Errors}
    !insertmacro SelectSection ${SecService}
  ${EndIf}
  ; Updates keep the shortcuts as they are.
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    !insertmacro UnselectSection ${SecDesktop}
    ${If} ${FileExists} "$DESKTOP\NyaRemoteControl 客户端.lnk"
    ${OrIf} ${FileExists} "$DESKTOP\NyaRemoteControl.lnk"
      !insertmacro SelectSection ${SecDesktop}
    ${EndIf}
  ${EndIf}
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

Section "Uninstall"
  Call un.EnsureAppClosed
  ; $APPDATA of all users: C:\ProgramData (the service's data).
  SetShellVarContext all
  nsExec::Exec 'sc query ${SERVICE}'
  Pop $0
  ${If} $0 == 0
  ${OrIf} ${FileExists} "$APPDATA\NyaRemoteControl\*.*"
    StrCpy $1 ""
    MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "是否同时删除本机被控用的配对信息、证书、设置和日志（$APPDATA\NyaRemoteControl）？$\r$\n$\r$\n选“否”则保留，重新安装后已配对的客户端不需要重新配对。" /SD IDNO IDNO +2
    StrCpy $1 " --purge"
    DetailPrint "删除远程控制服务…"
    nsExec::ExecToLog '"$INSTDIR\${CLI_EXE}" uninstall$1'
    Pop $0
  ${EndIf}
  Call un.StopHost

  !insertmacro DeleteShortcuts
  RMDir /r "$INSTDIR"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "Software\Classes\nyaremote"
  ; Start with Windows (set by the program for the user who turned it on).
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${APP_NAME}"
SectionEnd
