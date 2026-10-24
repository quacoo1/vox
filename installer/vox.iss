[Setup]
AppId={{8F1C2B6E-VOX0-4A1E-9C3D-RUSTREWRITE01}}
AppName=Vox
AppVersion=0.1.0
DefaultDirName={localappdata}\Programs\Vox
PrivilegesRequired=lowest
OutputBaseFilename=Vox-Setup-0.1.0
OutputDir=..\dist
Compression=lzma2
SolidCompression=yes
UninstallDisplayIcon={app}\vox.exe

[Files]
Source: "..\target\release\vox.exe"; DestDir: "{app}"
Source: "..\target\release\browser-extension\*"; DestDir: "{app}\browser-extension"; Flags: recursesubdirs

[Icons]
Name: "{userprograms}\Vox"; Filename: "{app}\vox.exe"

[Run]
Filename: "{app}\vox.exe"; Description: "Launch Vox"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "taskkill"; Parameters: "/IM vox.exe"; Flags: runhidden

[UninstallDelete]
Type: filesandordirs; Name: "{app}"
