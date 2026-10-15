# Windows' own voices (a third engine, never trained on): the phrase and
# near misses, at several speeds and pitches. Writes wintts/{pos,neg}_*.wav.
param([string]$Out = "$PSScriptRoot\..\wintts")
Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)
$pos = @("HasteClip that", "Haste clip that", "HasteClip that!", "Haste clip that.", "HasteClip, that!", "haste clip that?")
$neg = @("clip that", "paste clip that", "HasteClip", "hey clip that", "taste clip that", "HasteClip this", "just clip that", "haste", "hasty clip", "nice clip", "waste clip that", "HasteClip is open", "face clip that", "chase clip that", "did you clip that")
$rates = @(-2, 0, 2, 4, 6)
$pitches = @("x-low", "low", "medium", "high")
$i = 0
foreach ($v in @("Microsoft Zira Desktop", "Microsoft David Desktop")) {
  $s.SelectVoice($v)
  foreach ($r in $rates) {
    foreach ($p in $pitches) {
      foreach ($kind in @("pos", "neg")) {
        $list = if ($kind -eq "pos") { $pos } else { $neg }
        $t = $list[$i % $list.Count]
        $ssml = "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'><prosody pitch='$p'>$t</prosody></speak>"
        $s.Rate = $r
        $f = Join-Path $Out ("{0}_{1:d3}.wav" -f $kind, $i)
        $s.SetOutputToWaveFile($f, $fmt)
        $s.SpeakSsml($ssml)
        $s.SetOutputToNull()
      }
      $i++
    }
  }
}
"$i pairs"
