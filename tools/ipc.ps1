# Отправка команды службе SteadyBackup через именованный канал.
# Пример: .\tools\ipc.ps1 '{"cmd":"list_jobs"}'
param([Parameter(Mandatory)][string]$Json)

$pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'SteadyBackup', [System.IO.Pipes.PipeDirection]::InOut)
$pipe.Connect(3000)
$utf8 = New-Object System.Text.UTF8Encoding($false)
$w = New-Object System.IO.StreamWriter($pipe, $utf8); $w.AutoFlush = $true
$r = New-Object System.IO.StreamReader($pipe, $utf8)
$w.WriteLine($Json)
$r.ReadLine()
$pipe.Dispose()
