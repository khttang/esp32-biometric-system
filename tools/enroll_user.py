import os
import json
import numpy as np
import torch
import torchaudio
import onnxruntime as ort
from PIL import Image

# For speaker embeddings
try:
    from speechbrain.inference.speaker import EncoderClassifier
except ImportError:
    from speechbrain.pretrained import EncoderClassifier


class MultimodalBiometricEnroller:
    def __init__(self, face_model_path="face_net.onnx", db_path="enrolled_users.json"):
        self.db_path = db_path
        
        # 1. Initialize Face Recognition (ONNX)
        print("Loading Face Recognition Model...")
        self.face_session = ort.InferenceSession(face_model_path)
        self.face_input_name = self.face_session.get_inputs()[0].name

        # 2. Initialize Voice Recognition (ECAPA-TDNN via SpeechBrain)
        print("Loading Voice Speaker Embedding Model...")
        self.speaker_model = EncoderClassifier.from_hparams(
            source="speechbrain/spkrec-ecapa-voxceleb",
            savedir="tmp_speaker_model"
        )
        
        # 3. Load or create Database
        self.database = self._load_database()

    def _load_database(self):
        if os.path.exists(self.db_path):
            with open(self.db_path, "r") as f:
                return json.load(f)
        return {}

    # -------------------------------------------------------------
    # FACE EXTRACTION METHODS
    # -------------------------------------------------------------
    def _preprocess_face(self, image_path):
        img = Image.open(image_path).convert('RGB').resize((112, 112))
        arr = np.array(img, dtype=np.float32) / 255.0
        arr = (arr - 0.5) / 0.5  # Normalize to [-1, 1]
        arr = np.transpose(arr, (2, 0, 1))  # HWC -> NCHW
        return np.expand_dims(arr, axis=0)

    def extract_face_embedding(self, image_path):
        tensor = self._preprocess_face(image_path)
        outputs = self.face_session.run(None, {self.face_input_name: tensor})
        return outputs[0].flatten()

    # -------------------------------------------------------------
    # VOICE EXTRACTION METHODS
    # -------------------------------------------------------------
    def _preprocess_audio(self, audio_path, target_sample_rate=16000):
        """Loads WAV/audio, converts to mono, and resamples to 16kHz."""
        signal, fs = torchaudio.load(audio_path)
        
        # Convert stereo to mono if needed
        if signal.shape[0] > 1:
            signal = torch.mean(signal, dim=0, keepdim=True)
            
        # Resample to 16kHz required by ECAPA-TDNN
        if fs != target_sample_rate:
            resampler = torchaudio.transforms.Resample(orig_freq=fs, new_freq=target_sample_rate)
            signal = resampler(signal)
            
        return signal

    def extract_voice_embedding(self, audio_path):
        """Extracts a 192-d or 512-d speaker vector from an audio file."""
        signal = self._preprocess_audio(audio_path)
        
        # Encode speaker features using SpeechBrain
        with torch.no_grad():
            embeddings = self.speaker_model.encode_batch(signal)
            
        # Squeeze batch dimension and return 1D numpy array
        return embeddings.squeeze().cpu().numpy()

    # -------------------------------------------------------------
    # VECTOR NORMALIZATION & AVERAGING
    # -------------------------------------------------------------
    def _average_and_normalize(self, embeddings_list):
        if not embeddings_list:
            return None
        avg_emb = np.mean(embeddings_list, axis=0)
        norm = np.linalg.norm(avg_emb)
        if norm > 0:
            return (avg_emb / norm).tolist()
        return avg_emb.tolist()

    # -------------------------------------------------------------
    # ENROLLMENT PIPELINE
    # -------------------------------------------------------------
    def enroll_user(self, user_id, name, face_image_paths=None, voice_audio_paths=None):
        print(f"\n==========================================")
        print(f" Enrolling User: {name} (ID: {user_id})")
        print(f"==========================================")

        face_vector = None
        voice_vector = None

        # 1. Process Face Samples
        if face_image_paths:
            face_embeddings = []
            print(f"\n[Face] Processing {len(face_image_paths)} photo sample(s)...")
            for path in face_image_paths:
                if not os.path.exists(path):
                    print(f"  [Warning] Missing image file: {path}")
                    continue
                emb = self.extract_face_embedding(path)
                face_embeddings.append(emb)
                print(f"  ✓ Processed photo: {os.path.basename(path)}")

            face_vector = self._average_and_normalize(face_embeddings)

        # 2. Process Voice Samples
        if voice_audio_paths:
            voice_embeddings = []
            print(f"\n[Voice] Processing {len(voice_audio_paths)} audio sample(s)...")
            for path in voice_audio_paths:
                if not os.path.exists(path):
                    print(f"  [Warning] Missing audio file: {path}")
                    continue
                emb = self.extract_voice_embedding(path)
                voice_embeddings.append(emb)
                print(f"  ✓ Processed audio: {os.path.basename(path)}")

            voice_vector = self._average_and_normalize(voice_embeddings)

        # 3. Store Multimodal Profile
        if user_id not in self.database:
            self.database[user_id] = {"name": name}

        self.database[user_id]["name"] = name
        if face_vector is not None:
            self.database[user_id]["face_embedding"] = face_vector
            self.database[user_id]["face_samples_count"] = len(face_image_paths)
        if voice_vector is not None:
            self.database[user_id]["voice_embedding"] = voice_vector
            self.database[user_id]["voice_samples_count"] = len(voice_audio_paths)

        # 4. Save Database to File
        with open(self.db_path, "w") as f:
            json.dump(self.database, f, indent=2)

        print(f"\nSuccess! Profile for '{name}' updated in '{self.db_path}'.")


if __name__ == "__main__":
    # Example usage:
    enroller = MultimodalBiometricEnroller(face_model_path="face_net.onnx")

    # Define paths to sample photos and sample audio recordings
    user_photos = ["alice_1.jpg", "alice_2.jpg", "alice_3.jpg"]
    user_audio = ["alice_speech_1.wav", "alice_speech_2.wav"]

    # Enroll user with both face and voice vectors
    # enroller.enroll_user(
    #     user_id="usr_001",
    #     name="Alice",
    #     face_image_paths=user_photos,
    #     voice_audio_paths=user_audio
    # )
