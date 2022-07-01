import pymongo
import os
from dotenv import load_dotenv
load_dotenv()

connectionString = os.getenv('DB_CONNECT_STRING')
client = pymongo.MongoClient(connectionString)
db = client.StarshipDB
ServerConfigs = {}

def updateConfig(guildID):
    config = ServerConfigs[guildID]
    if config is None:
        return
    db.ServerConfigs.update_one(
        {'guildID': guildID},
        {'$set': config},
        upsert=True
    )

def deleteConfig(guildID):
    db.ServerConfigs.delete_one({ "guildID": guildID })
